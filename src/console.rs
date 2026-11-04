//! Text console on the framebuffer.
//!
//! The firmware's text console is gone after ExitBootServices. What's left is
//! the framebuffer: a block of memory where each `u32` is one pixel on screen.
//! To show text we copy each character's pixels from a bitmap font.
//!
//! Everything printed with [`print!`]/[`println!`]/[`cprint!`] also goes to the
//! serial port, so the output is visible even without a screen. Colors and
//! clearing the screen are sent there as ANSI escape sequences, which every
//! terminal understands.

use alloc::vec;
use alloc::vec::Vec;
use core::fmt::{self, Write};
use noto_sans_mono_bitmap::{FontWeight, RasterHeight, get_raster, get_raster_width};
use spin::Mutex;

const FONT_SIZE: RasterHeight = RasterHeight::Size20;
/// Character cell height in pixels.
const CELL_H: usize = FONT_SIZE.val();
/// Character cell width in pixels (the font is monospaced).
const CELL_W: usize = get_raster_width(FontWeight::Regular, FONT_SIZE);
/// Cursor bar: 2 pixels high, 1 pixel above the bottom of the cell.
const CURSOR_H: usize = 2;
const CURSOR_GAP: usize = 1;

/// Background color (0xRRGGBB).
pub const BACKGROUND: u32 = 0x0c0f1a;
/// Default text color (0xRRGGBB).
pub const FOREGROUND: u32 = 0xc8cdd8;

/// 16-color palette (0xRRGGBB), like the classic VGA colors but softer.
pub const PALETTE: [u32; 16] = [
    0x000000, 0x3b6fd9, 0x3fae5a, 0x2aa7b8, 0xc94646, 0xa35bc9, 0xb58a2e, 0xc8cdd8, //
    0x6b7280, 0x6f9bff, 0x7ee08f, 0x6fe3f2, 0xff7b7b, 0xe08cff, 0xf5d547, 0xffffff,
];

/// Named colors from [`PALETTE`].
#[allow(missing_docs)]
pub mod color {
    use super::PALETTE;
    pub const CYAN: u32 = PALETTE[3];
    pub const MAGENTA: u32 = PALETTE[5];
    pub const GRAY: u32 = PALETTE[8];
    pub const LIGHT_BLUE: u32 = PALETTE[9];
    pub const LIGHT_GREEN: u32 = PALETTE[10];
    pub const LIGHT_CYAN: u32 = PALETTE[11];
    pub const LIGHT_RED: u32 = PALETTE[12];
    pub const LIGHT_MAGENTA: u32 = PALETTE[13];
    pub const YELLOW: u32 = PALETTE[14];
    pub const WHITE: u32 = PALETTE[15];
}

/// Framebuffer description, captured from the firmware's Graphics Output Protocol.
#[derive(Clone, Copy, Debug)]
pub struct FrameBufferInfo {
    /// Physical address of the first pixel.
    pub base: u64,
    /// Visible width in pixels.
    pub width: usize,
    /// Visible height in pixels.
    pub height: usize,
    /// Pixels per row in memory (can be larger than `width`).
    pub stride: usize,
    /// Byte order of a pixel: blue-green-red (true) or red-green-blue.
    pub bgr: bool,
}

/// A text console drawing into a framebuffer.
#[derive(Debug)]
pub struct Console {
    fb: FrameBufferInfo,
    /// Copy of the screen in normal RAM. Video memory is slow to read,
    /// so scrolling works on this copy and then pushes it to the screen.
    shadow: Vec<u32>,
    cols: usize,
    rows: usize,
    col: usize,
    row: usize,
    fg: u32,
    bold: bool,
    cursor_drawn: bool,
}

impl Console {
    /// Create a console and clear the screen.
    ///
    /// # Safety
    /// `fb` must describe a writable framebuffer of at least
    /// `stride * height` pixels that stays valid for the console's lifetime.
    unsafe fn new(fb: FrameBufferInfo) -> Self {
        let mut console = Console {
            fb,
            shadow: vec![0; fb.width * fb.height],
            cols: fb.width / CELL_W,
            rows: fb.height / CELL_H,
            col: 0,
            row: 0,
            fg: FOREGROUND,
            bold: false,
            cursor_drawn: false,
        };
        console.clear();
        console
    }

    /// Convert 0xRRGGBB to the framebuffer's pixel layout.
    fn pixel(&self, rgb: u32) -> u32 {
        if self.fb.bgr { rgb } else { ((rgb & 0xff) << 16) | (rgb & 0xff00) | ((rgb >> 16) & 0xff) }
    }

    /// Copy a rectangle from the shadow buffer to video memory.
    ///
    /// Each row is then written back from the CPU cache: on the Raspberry Pi 5
    /// the display engine reads the framebuffer straight from RAM.
    fn flush(&self, x: usize, y: usize, w: usize, h: usize) {
        let fb = self.fb.base as *mut u32;
        for row in y..y + h {
            let src = &self.shadow[row * self.fb.width + x..][..w];
            let start = row * self.fb.stride + x;
            for (i, &px) in src.iter().enumerate() {
                // SAFETY: `row < height` and `x + i < width <= stride`, so the
                // pixel lies inside the framebuffer promised to `Console::new`.
                unsafe { fb.add(start + i).write_volatile(px) };
            }
            crate::arch::dma_clean(self.fb.base + start as u64 * 4, w * 4);
        }
    }

    fn fill_rect(&mut self, x: usize, y: usize, w: usize, h: usize, rgb: u32) {
        let px = self.pixel(rgb);
        for row in y..y + h {
            self.shadow[row * self.fb.width + x..][..w].fill(px);
        }
        self.flush(x, y, w, h);
    }

    fn draw_char(&mut self, c: char) {
        let weight = if self.bold { FontWeight::Bold } else { FontWeight::Regular };
        let Some(glyph) = get_raster(c, weight, FONT_SIZE).or_else(|| get_raster('?', weight, FONT_SIZE))
        else {
            return;
        };
        let (x0, y0) = (self.col * CELL_W, self.row * CELL_H);
        for (y, line) in glyph.rows().take(CELL_H).enumerate() {
            for (x, &intensity) in line.iter().take(CELL_W).enumerate() {
                // Font pixels are grayscale (0..255): blend fg over bg for smooth edges.
                let rgb = blend(BACKGROUND, self.fg, intensity);
                self.shadow[(y0 + y) * self.fb.width + x0 + x] = self.pixel(rgb);
            }
        }
        self.flush(x0, y0, CELL_W, CELL_H);
    }

    fn scroll(&mut self) {
        let line = CELL_H * self.fb.width;
        let used = self.rows * line;
        self.shadow.copy_within(line..used, 0);
        let bg = self.pixel(BACKGROUND);
        self.shadow[used - line..used].fill(bg);
        self.flush(0, 0, self.fb.width, self.rows * CELL_H);
    }

    fn newline(&mut self) {
        self.col = 0;
        self.row += 1;
        if self.row == self.rows {
            self.scroll();
            self.row = self.rows - 1;
        }
    }

    fn put_char(&mut self, c: char) {
        match c {
            '\n' => self.newline(),
            '\r' => self.col = 0,
            // Backspace moves the cursor left without erasing; from the start
            // of a row it goes back to the end of the previous one, so the
            // line editor can move through a command that wrapped.
            '\x08' => {
                if self.col > 0 {
                    self.col -= 1;
                } else if self.row > 0 {
                    self.row -= 1;
                    self.col = self.cols - 1;
                }
            }
            '\t' => {
                for _ in 0..4 - self.col % 4 {
                    self.put_char(' ');
                }
            }
            _ => {
                if self.col >= self.cols {
                    self.newline();
                }
                self.draw_char(c);
                self.col += 1;
            }
        }
    }

    /// Clear the screen and move the cursor to the top left.
    pub fn clear(&mut self) {
        self.cursor_drawn = false;
        self.fill_rect(0, 0, self.fb.width, self.fb.height, BACKGROUND);
        self.col = 0;
        self.row = 0;
    }

    /// Draw or erase the cursor bar under the current cell.
    pub fn show_cursor(&mut self, on: bool) {
        if on == self.cursor_drawn || self.col >= self.cols {
            return;
        }
        let (x, y) = (self.col * CELL_W, self.row * CELL_H);
        let rgb = if on { self.fg } else { BACKGROUND };
        self.fill_rect(x, y + CELL_H - CURSOR_H - CURSOR_GAP, CELL_W, CURSOR_H, rgb);
        self.cursor_drawn = on;
    }

    /// Set the text color (0xRRGGBB).
    pub fn set_fg(&mut self, rgb: u32) {
        self.fg = rgb;
    }

    /// Current text color (0xRRGGBB).
    pub fn fg(&self) -> u32 {
        self.fg
    }

    /// Switch between the regular and the bold font.
    pub fn set_bold(&mut self, bold: bool) {
        self.bold = bold;
    }

    /// Size in characters: `(columns, rows)`.
    pub fn size(&self) -> (usize, usize) {
        (self.cols, self.rows)
    }

    /// Size in pixels: `(width, height)`.
    pub fn resolution(&self) -> (usize, usize) {
        (self.fb.width, self.fb.height)
    }
}

impl Write for Console {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.show_cursor(false);
        for c in s.chars() {
            self.put_char(c);
        }
        Ok(())
    }
}

/// Mix `fg` over `bg` with coverage `alpha` (0 = all bg, 255 = all fg).
fn blend(bg: u32, fg: u32, alpha: u8) -> u32 {
    let a = alpha as u32;
    let mix = |shift: u32| {
        let b = (bg >> shift) & 0xff;
        let f = (fg >> shift) & 0xff;
        ((f * a + b * (255 - a)) / 255) << shift
    };
    mix(16) | mix(8) | mix(0)
}

/// The global console. `None` until [`init`] runs (or if there is no screen).
pub static CONSOLE: Mutex<Option<Console>> = Mutex::new(None);

/// Start drawing text on the framebuffer.
///
/// # Safety
/// Same as for the framebuffer in [`Console::new`]: `fb` must describe valid,
/// writable video memory that stays mapped forever.
pub unsafe fn init(fb: FrameBufferInfo) {
    // SAFETY: forwarded from the caller.
    *CONSOLE.lock() = Some(unsafe { Console::new(fb) });
}

/// Clear the screen, and the serial terminal too.
pub fn clear() {
    let _ = crate::serial::Serial.write_str("\x1b[2J\x1b[H"); // erase all, cursor home
    with(Console::clear);
}

/// Run `f` on the console, if there is one.
pub fn with<R>(f: impl FnOnce(&mut Console) -> R) -> Option<R> {
    CONSOLE.lock().as_mut().map(f)
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    let _ = crate::serial::Serial.write_fmt(args);
    with(|c| c.write_fmt(args));
}

/// ANSI "select graphic rendition" code for a [`PALETTE`] color, e.g. 91
/// for light red. The palette is in VGA order, ANSI swaps red and blue.
fn ansi_color(rgb: u32) -> Option<u8> {
    const VGA_TO_ANSI: [u8; 8] = [0, 4, 2, 6, 1, 5, 3, 7];
    let i = PALETTE.iter().position(|&c| c == rgb)?;
    Some(if i < 8 { 30 + VGA_TO_ANSI[i] } else { 90 + VGA_TO_ANSI[i - 8] })
}

/// Print in a given color, then switch back.
#[doc(hidden)]
pub fn _print_colored(rgb: u32, args: fmt::Arguments) {
    let mut serial = crate::serial::Serial;
    let _ = match ansi_color(rgb) {
        // 39 = the terminal's default color.
        Some(code) => write!(serial, "\x1b[{code}m{args}\x1b[39m"),
        None => serial.write_fmt(args),
    };
    with(|c| {
        let old = c.fg();
        c.set_fg(rgb);
        let _ = c.write_fmt(args);
        c.set_fg(old);
    });
}

/// Print to the screen and the serial port.
macro_rules! print {
    ($($arg:tt)*) => { $crate::console::_print(format_args!($($arg)*)) };
}

/// Print a line to the screen and the serial port.
macro_rules! println {
    () => { $crate::console::_print(format_args!("\n")) };
    ($($arg:tt)*) => { $crate::console::_print(format_args!("{}\n", format_args!($($arg)*))) };
}

/// Print in a color: `cprint!(color::LIGHT_RED, "...")`.
macro_rules! cprint {
    ($color:expr, $($arg:tt)*) => { $crate::console::_print_colored($color, format_args!($($arg)*)) };
}

pub(crate) use {cprint, print, println};

#[cfg(test)]
mod tests {
    use super::*;

    /// A console drawing into a heap buffer instead of video memory.
    fn console(cols: usize, rows: usize, buffer: &mut Vec<u32>) -> Console {
        let (width, height) = (cols * CELL_W, rows * CELL_H);
        *buffer = vec![0; width * height];
        let fb =
            FrameBufferInfo { base: buffer.as_mut_ptr() as u64, width, height, stride: width, bgr: true };
        // SAFETY: `buffer` holds exactly `stride * height` pixels and outlives the console.
        unsafe { Console::new(fb) }
    }

    #[test]
    fn blend_mixes_channels() {
        assert_eq!(blend(0x000000, 0xffffff, 0), 0x000000);
        assert_eq!(blend(0x000000, 0xffffff, 255), 0xffffff);
        assert_eq!(blend(0x000000, 0x00ff00, 51), 0x003300);
    }

    #[test]
    fn writes_wrap_and_scroll() {
        let mut fb = Vec::new();
        let mut c = console(4, 2, &mut fb);
        assert!(fb.iter().all(|&px| px == BACKGROUND));
        c.write_str("ab").unwrap();
        assert_eq!((c.col, c.row), (2, 0));
        c.write_str("cdef").unwrap(); // wraps after 4 columns
        assert_eq!((c.col, c.row), (2, 1));
        c.write_str("\n\n").unwrap(); // scrolls instead of leaving the screen
        assert_eq!((c.col, c.row), (0, 1));
        c.write_str("\t").unwrap(); // tab to the next multiple of 4
        assert_eq!(c.col, 4);
        c.write_str("x\x08").unwrap(); // wraps, then backspace
        assert_eq!((c.col, c.row), (0, 1));
    }

    #[test]
    fn backspace_moves_back_across_wrapped_rows() {
        let mut fb = Vec::new();
        let mut c = console(4, 3, &mut fb);
        c.write_str("abcdef").unwrap(); // "abcd" on row 0, "ef" on row 1
        assert_eq!((c.col, c.row), (2, 1));
        c.write_str("\x08\x08\x08").unwrap();
        assert_eq!((c.col, c.row), (3, 0)); // on the "d"
        c.write_str("D").unwrap(); // overwrite it; the wrap is deferred...
        assert_eq!((c.col, c.row), (4, 0));
        c.write_str("\x08").unwrap(); // ...so backspace stays on the row
        assert_eq!((c.col, c.row), (3, 0));
        c.write_str("\x08\x08\x08\x08").unwrap(); // stops at the top left
        assert_eq!((c.col, c.row), (0, 0));
    }

    #[test]
    fn palette_colors_map_to_ansi_codes() {
        assert_eq!(ansi_color(color::LIGHT_RED), Some(91));
        assert_eq!(ansi_color(color::LIGHT_BLUE), Some(94));
        assert_eq!(ansi_color(color::CYAN), Some(36));
        assert_eq!(ansi_color(color::GRAY), Some(90));
        assert_eq!(ansi_color(0x123456), None);
    }

    #[test]
    fn rgb_is_swapped_for_rgb_framebuffers() {
        let mut fb = Vec::new();
        let mut c = console(1, 1, &mut fb);
        assert_eq!(c.pixel(0x112233), 0x112233);
        c.fb.bgr = false;
        assert_eq!(c.pixel(0x112233), 0x332211);
    }
}
