//! Turns USB keyboard reports into key presses.
//!
//! A HID "boot protocol" report is 8 bytes: [modifiers, reserved, key1..key6].
//! Keys are "usage IDs" (positions on the keyboard, not characters), so the
//! layout table decides which character a key produces. The report lists keys
//! that are *currently held*; a key press is a key that wasn't there before.
//!
//! The serial console is a second keyboard. Terminals send plain bytes,
//! UTF-8 for non-ASCII characters and ANSI escape sequences for special keys
//! (`ESC [ A` is the up arrow); [`SerialDecoder`] turns those into the same
//! [`Key`]s.
//!
//! References: USB HID Usage Tables 1.5, chapter 10 ("Keyboard/Keypad Page");
//! ECMA-48 (control functions, the "ANSI" escape sequences).

use crate::interrupts::uptime_ms;
use crate::serial;
use crate::xhci::Xhci;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::Mutex;

/// A key press, after applying the keyboard layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum Key {
    /// A printable character.
    Char(char),
    Enter,
    Backspace,
    Tab,
    Escape,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    Delete,
    /// A letter typed with Ctrl held, e.g. `Ctrl('c')`.
    Ctrl(char),
}

/// Keyboard layout: which character each key position produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Layout {
    /// US English (QWERTY).
    Us,
    /// German (QWERTZ), with umlauts and AltGr symbols.
    De,
}

// Modifier byte bits.
const CTRL: u8 = 0x01 | 0x10; // left or right control
const SHIFT: u8 = 0x02 | 0x20; // left or right shift
const ALT_GR: u8 = 0x40; // right alt

/// Hold a key this long before it starts repeating...
const REPEAT_DELAY_MS: u64 = 500;
/// ...then repeat it at this interval.
const REPEAT_RATE_MS: u64 = 40;

/// Usage ID reported in all six key slots when too many keys are pressed.
const ERROR_ROLL_OVER: u8 = 0x01;

/// A lone ESC byte is the Escape key, unless more bytes of an escape
/// sequence follow within this time.
const ESC_TIMEOUT_MS: u64 = 100;

/// The running USB controllers (set up by `kernel_main`). A Raspberry Pi 5
/// has two, each serving one USB 2 and one USB 3 port.
pub static USB: Mutex<Vec<Xhci>> = Mutex::new(Vec::new());

#[derive(Debug)]
struct State {
    layout: Layout,
    held: [u8; 6],
    /// Key being held down for auto-repeat: (usage, key, next repeat time).
    repeat: Option<(u8, Key, u64)>,
    serial: SerialDecoder,
    queue: VecDeque<Key>,
}

static STATE: Mutex<State> = Mutex::new(State::new());

/// Switch the keyboard layout.
pub fn set_layout(layout: Layout) {
    STATE.lock().layout = layout;
}

/// The current keyboard layout.
pub fn layout() -> Layout {
    STATE.lock().layout
}

/// Next key press, if any. Never blocks.
pub fn poll() -> Option<Key> {
    let mut state = STATE.lock();
    state.collect();
    state.queue.pop_front()
}

/// Whether Ctrl-C was pressed: removes it from the input queue, leaving the
/// other keys for later. Lets long-running commands stop early.
pub fn interrupted() -> bool {
    let mut state = STATE.lock();
    state.collect();
    let position = state.queue.iter().position(|&k| k == Key::Ctrl('c'));
    position.and_then(|i| state.queue.remove(i)).is_some()
}

impl State {
    const fn new() -> Self {
        State {
            layout: Layout::Us,
            held: [0; 6],
            repeat: None,
            serial: SerialDecoder::new(),
            queue: VecDeque::new(),
        }
    }

    /// Move new input from all keyboards into the queue.
    fn collect(&mut self) {
        for usb in USB.lock().iter_mut() {
            while let Some(report) = usb.poll() {
                self.on_report(&report);
            }
        }
        // The serial terminal, e.g. `make qemu` or a USB-serial adapter.
        let now = uptime_ms();
        while let Some(byte) = serial::read_byte() {
            self.serial.feed(byte, now, &mut self.queue);
        }
        self.serial.on_tick(now, &mut self.queue);
        self.on_tick(now);
    }

    /// Auto-repeat the held key once its next repeat time `now` has come.
    fn on_tick(&mut self, now: u64) {
        if let Some((usage, key, next)) = self.repeat
            && now >= next
        {
            self.queue.push_back(key);
            self.repeat = Some((usage, key, now + REPEAT_RATE_MS));
        }
    }

    /// Process one boot-protocol report: `[modifiers, reserved, key1..key6]`.
    fn on_report(&mut self, report: &[u8; 8]) {
        self.on_report_at(report, uptime_ms());
    }

    fn on_report_at(&mut self, report: &[u8; 8], now: u64) {
        let modifiers = report[0];
        let keys = &report[2..8];
        if keys.iter().all(|&k| k == ERROR_ROLL_OVER) {
            return; // "phantom" report: too many keys pressed at once
        }
        for &usage in keys {
            if usage == 0 || self.held.contains(&usage) {
                continue;
            }
            if let Some(key) = translate(self.layout, usage, modifiers) {
                self.queue.push_back(key);
                self.repeat = Some((usage, key, now + REPEAT_DELAY_MS));
            }
        }
        // Stop repeating once the key is released.
        if let Some((usage, _, _)) = self.repeat
            && !keys.contains(&usage)
        {
            self.repeat = None;
        }
        self.held.copy_from_slice(keys);
    }
}

/// Turns bytes from a serial terminal into [`Key`]s.
///
/// Most bytes are characters, but three things need more than one byte:
/// - escape sequences for special keys: `ESC [ A` (up), `ESC [ 3 ~` (delete),
///   `ESC O H` (home, in some terminals' "application mode"), ...
/// - UTF-8: `ü` arrives as the two bytes `0xc3 0xbc`.
/// - the Escape key itself is a lone `ESC`, told apart from the start of a
///   sequence by the pause after it ([`ESC_TIMEOUT_MS`]).
#[derive(Debug)]
struct SerialDecoder {
    /// Bytes of an unfinished escape sequence or UTF-8 character.
    pending: [u8; 8],
    len: usize,
    /// The escape sequence has more parameters than fit: ignore it.
    overflow: bool,
    /// When the pending ESC arrived.
    esc_at: u64,
}

const ESC: u8 = 0x1b;

impl SerialDecoder {
    const fn new() -> Self {
        SerialDecoder { pending: [0; 8], len: 0, overflow: false, esc_at: 0 }
    }

    /// Process one byte received at time `now`; finished keys go to `out`.
    fn feed(&mut self, byte: u8, now: u64, out: &mut VecDeque<Key>) {
        if self.len == 0 {
            return self.start(byte, now, out);
        }
        if self.pending[0] == ESC {
            self.feed_escape(byte, now, out);
        } else {
            self.feed_utf8(byte, now, out);
        }
    }

    /// A byte that doesn't continue an earlier one.
    fn start(&mut self, byte: u8, now: u64, out: &mut VecDeque<Key>) {
        let key = match byte {
            b'\r' | b'\n' => Key::Enter,
            0x08 | 0x7f => Key::Backspace,
            b'\t' => Key::Tab,
            0x01..=0x1a => Key::Ctrl((b'a' + byte - 1) as char), // Ctrl-A .. Ctrl-Z
            ESC | 0xc2..=0xf4 => {
                // Start of an escape sequence or a multi-byte UTF-8 character.
                self.pending[0] = byte;
                self.len = 1;
                self.esc_at = now;
                return;
            }
            0x20..=0x7e => Key::Char(byte as char),
            _ => return, // other control bytes, stray UTF-8 continuation bytes
        };
        out.push_back(key);
    }

    fn feed_escape(&mut self, byte: u8, now: u64, out: &mut VecDeque<Key>) {
        if self.len == 1 {
            if byte == b'[' || byte == b'O' {
                self.push(byte);
            } else {
                // Not a sequence: the Escape key, then an ordinary byte.
                self.len = 0;
                out.push_back(Key::Escape);
                self.start(byte, now, out);
            }
            return;
        }
        match byte {
            // Parameter bytes, e.g. the "3" in `ESC [ 3 ~`.
            0x30..=0x3f if self.len < self.pending.len() => self.push(byte),
            0x30..=0x3f => self.overflow = true,
            // Final byte: the sequence is complete.
            0x40..=0x7e => {
                let params = &self.pending[2..self.len];
                if let Some(key) = escape_key(params, byte).filter(|_| !self.overflow) {
                    out.push_back(key);
                }
                self.reset();
            }
            _ => self.reset(), // malformed: drop it
        }
    }

    fn feed_utf8(&mut self, byte: u8, now: u64, out: &mut VecDeque<Key>) {
        if byte & 0xc0 != 0x80 {
            // Not a continuation byte: drop the broken character.
            self.len = 0;
            return self.start(byte, now, out);
        }
        self.push(byte);
        let lead = self.pending[0];
        let needed = if lead >= 0xf0 {
            4
        } else if lead >= 0xe0 {
            3
        } else {
            2
        };
        if self.len == needed {
            let c = core::str::from_utf8(&self.pending[..needed]).ok().and_then(|s| s.chars().next());
            out.extend(c.map(Key::Char));
            self.len = 0;
        }
    }

    fn reset(&mut self) {
        self.len = 0;
        self.overflow = false;
    }

    fn push(&mut self, byte: u8) {
        self.pending[self.len] = byte;
        self.len += 1;
    }

    /// A lone ESC with nothing after it for a while is the Escape key.
    fn on_tick(&mut self, now: u64, out: &mut VecDeque<Key>) {
        if self.len == 1 && self.pending[0] == ESC && now >= self.esc_at + ESC_TIMEOUT_MS {
            self.len = 0;
            out.push_back(Key::Escape);
        }
    }
}

/// The key for a complete escape sequence `ESC [ <params> <last>` (or
/// `ESC O <last>`), as xterm and most other terminals send them.
fn escape_key(params: &[u8], last: u8) -> Option<Key> {
    Some(match (params, last) {
        (_, b'A') => Key::Up,
        (_, b'B') => Key::Down,
        (_, b'C') => Key::Right,
        (_, b'D') => Key::Left,
        (_, b'H') | (b"1" | b"7", b'~') => Key::Home,
        (_, b'F') | (b"4" | b"8", b'~') => Key::End,
        (b"3", b'~') => Key::Delete,
        _ => return None,
    })
}

/// Turn a key position (usage ID) plus modifiers into a [`Key`].
fn translate(layout: Layout, usage: u8, modifiers: u8) -> Option<Key> {
    let shift = modifiers & SHIFT != 0;
    let alt_gr = modifiers & ALT_GR != 0;
    let ctrl = modifiers & CTRL != 0;
    let key = match usage {
        0x28 | 0x58 => Key::Enter,
        0x29 => Key::Escape,
        0x2a => Key::Backspace,
        0x2b => Key::Tab,
        0x4a => Key::Home,
        0x4c => Key::Delete,
        0x4d => Key::End,
        0x4f => Key::Right,
        0x50 => Key::Left,
        0x51 => Key::Down,
        0x52 => Key::Up,
        // Keypad (always as digits: ROS doesn't track Num Lock).
        0x54..=0x63 => Key::Char(b"/*-+\n1234567890."[(usage - 0x54) as usize] as char),
        _ => {
            let c = match layout {
                Layout::Us => us(usage, shift),
                Layout::De => de(usage, shift, alt_gr),
            }?;
            if ctrl && c.is_ascii_alphabetic() { Key::Ctrl(c.to_ascii_lowercase()) } else { Key::Char(c) }
        }
    };
    Some(key)
}

/// Letters a-z are usage IDs 0x04-0x1d in alphabetical order.
fn letter(usage: u8, shift: bool) -> char {
    let c = (b'a' + usage - 0x04) as char;
    if shift { c.to_ascii_uppercase() } else { c }
}

/// US English (QWERTY) layout.
fn us(usage: u8, shift: bool) -> Option<char> {
    const DIGITS: &[u8; 10] = b"1234567890";
    const SHIFTED: &[u8; 10] = b"!@#$%^&*()";
    let pick = |a: char, b: char| if shift { b } else { a };
    Some(match usage {
        0x04..=0x1d => letter(usage, shift),
        0x1e..=0x27 => {
            let i = (usage - 0x1e) as usize;
            (if shift { SHIFTED[i] } else { DIGITS[i] }) as char
        }
        0x2c => ' ',
        0x2d => pick('-', '_'),
        0x2e => pick('=', '+'),
        0x2f => pick('[', '{'),
        0x30 => pick(']', '}'),
        0x31 | 0x64 => pick('\\', '|'),
        0x32 => pick('#', '~'),
        0x33 => pick(';', ':'),
        0x34 => pick('\'', '"'),
        0x35 => pick('`', '~'),
        0x36 => pick(',', '<'),
        0x37 => pick('.', '>'),
        0x38 => pick('/', '?'),
        _ => return None,
    })
}

/// German (QWERTZ) layout: Y and Z swapped, umlauts, AltGr symbols.
fn de(usage: u8, shift: bool, alt_gr: bool) -> Option<char> {
    const DIGITS: &[char; 10] = &['1', '2', '3', '4', '5', '6', '7', '8', '9', '0'];
    const SHIFTED: &[char; 10] = &['!', '"', '§', '$', '%', '&', '/', '(', ')', '='];
    let pick = |a: char, b: char| if shift { b } else { a };
    if alt_gr {
        return Some(match usage {
            0x14 => '@',
            0x24 => '{',
            0x25 => '[',
            0x26 => ']',
            0x27 => '}',
            0x2d => '\\',
            0x30 => '~',
            0x64 => '|',
            _ => return None,
        });
    }
    Some(match usage {
        0x1c => letter(0x1d, shift), // Y key position gives z
        0x1d => letter(0x1c, shift), // Z key position gives y
        0x04..=0x1b => letter(usage, shift),
        0x1e..=0x27 => {
            let i = (usage - 0x1e) as usize;
            if shift { SHIFTED[i] } else { DIGITS[i] }
        }
        0x2c => ' ',
        0x2d => pick('ß', '?'),
        0x2e => pick('´', '`'),
        0x2f => pick('ü', 'Ü'),
        0x30 => pick('+', '*'),
        0x31 | 0x32 => pick('#', '\''),
        0x33 => pick('ö', 'Ö'),
        0x34 => pick('ä', 'Ä'),
        0x35 => pick('^', '°'),
        0x36 => pick(',', ';'),
        0x37 => pick('.', ':'),
        0x38 => pick('-', '_'),
        0x64 => pick('<', '>'),
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    const LEFT_SHIFT: u8 = 0x02;

    fn report(modifiers: u8, keys: &[u8]) -> [u8; 8] {
        let mut r = [0u8; 8];
        r[0] = modifiers;
        r[2..2 + keys.len()].copy_from_slice(keys);
        r
    }

    fn drain(state: &mut State) -> Vec<Key> {
        state.queue.drain(..).collect()
    }

    #[test]
    fn us_layout() {
        assert_eq!(translate(Layout::Us, 0x04, 0), Some(Key::Char('a')));
        assert_eq!(translate(Layout::Us, 0x04, LEFT_SHIFT), Some(Key::Char('A')));
        assert_eq!(translate(Layout::Us, 0x1d, 0), Some(Key::Char('z')));
        assert_eq!(translate(Layout::Us, 0x1e, LEFT_SHIFT), Some(Key::Char('!')));
        assert_eq!(translate(Layout::Us, 0x27, 0), Some(Key::Char('0')));
        assert_eq!(translate(Layout::Us, 0x28, 0), Some(Key::Enter));
        assert_eq!(translate(Layout::Us, 0x52, 0), Some(Key::Up));
        assert_eq!(translate(Layout::Us, 0x3a, 0), None); // F1
    }

    #[test]
    fn german_layout() {
        assert_eq!(translate(Layout::De, 0x1c, 0), Some(Key::Char('z'))); // Y position
        assert_eq!(translate(Layout::De, 0x1d, 0), Some(Key::Char('y'))); // Z position
        assert_eq!(translate(Layout::De, 0x33, LEFT_SHIFT), Some(Key::Char('Ö')));
        assert_eq!(translate(Layout::De, 0x1f, LEFT_SHIFT), Some(Key::Char('"')));
        assert_eq!(translate(Layout::De, 0x14, ALT_GR), Some(Key::Char('@')));
        assert_eq!(translate(Layout::De, 0x04, ALT_GR), None);
    }

    #[test]
    fn reports_only_new_keys() {
        let mut s = State::new();
        s.on_report_at(&report(0, &[0x0b]), 0); // h
        s.on_report_at(&report(0, &[0x0b, 0x0c]), 10); // h still held, i new
        s.on_report_at(&report(0, &[]), 20); // all released
        assert_eq!(drain(&mut s), [Key::Char('h'), Key::Char('i')]);
        assert_eq!(s.repeat, None);
    }

    #[test]
    fn ignores_phantom_reports() {
        let mut s = State::new();
        s.on_report_at(&report(0, &[ERROR_ROLL_OVER; 6]), 0);
        assert!(drain(&mut s).is_empty());
    }

    #[test]
    fn repeats_held_key_after_delay() {
        let mut s = State::new();
        s.on_report_at(&report(0, &[0x04]), 0);
        s.on_tick(REPEAT_DELAY_MS - 1);
        assert_eq!(drain(&mut s), [Key::Char('a')]);
        s.on_tick(REPEAT_DELAY_MS);
        s.on_tick(REPEAT_DELAY_MS + REPEAT_RATE_MS);
        assert_eq!(drain(&mut s), [Key::Char('a'), Key::Char('a')]);
    }

    #[test]
    fn ctrl_keys_keypad_and_navigation() {
        const LEFT_CTRL: u8 = 0x01;
        assert_eq!(translate(Layout::Us, 0x06, LEFT_CTRL), Some(Key::Ctrl('c')));
        assert_eq!(translate(Layout::Us, 0x06, LEFT_CTRL | LEFT_SHIFT), Some(Key::Ctrl('c')));
        assert_eq!(translate(Layout::De, 0x1c, LEFT_CTRL), Some(Key::Ctrl('z'))); // Y position
        assert_eq!(translate(Layout::Us, 0x1e, LEFT_CTRL), Some(Key::Char('1'))); // not a letter
        assert_eq!(translate(Layout::Us, 0x59, 0), Some(Key::Char('1'))); // keypad 1
        assert_eq!(translate(Layout::Us, 0x62, 0), Some(Key::Char('0'))); // keypad 0
        assert_eq!(translate(Layout::Us, 0x57, 0), Some(Key::Char('+')));
        assert_eq!(translate(Layout::Us, 0x58, 0), Some(Key::Enter)); // keypad enter
        assert_eq!(translate(Layout::Us, 0x4a, 0), Some(Key::Home));
        assert_eq!(translate(Layout::Us, 0x4c, 0), Some(Key::Delete));
        assert_eq!(translate(Layout::Us, 0x4d, 0), Some(Key::End));
    }

    /// Feed `bytes` to a serial decoder at time 0 and return the keys.
    fn decode(bytes: &[u8]) -> Vec<Key> {
        let mut d = SerialDecoder::new();
        let mut out = VecDeque::new();
        for &b in bytes {
            d.feed(b, 0, &mut out);
        }
        out.into_iter().collect()
    }

    #[test]
    fn serial_bytes() {
        assert_eq!(decode(b"a\r\x7f\t"), [Key::Char('a'), Key::Enter, Key::Backspace, Key::Tab]);
        assert_eq!(decode(b"\x03\x0c"), [Key::Ctrl('c'), Key::Ctrl('l')]);
        assert_eq!(decode(b"\x00\x1c"), []); // other control bytes are ignored
    }

    #[test]
    fn serial_escape_sequences() {
        assert_eq!(decode(b"\x1b[A\x1b[B\x1b[C\x1b[D"), [Key::Up, Key::Down, Key::Right, Key::Left]);
        assert_eq!(decode(b"\x1b[H\x1b[F\x1bOH\x1bOF"), [Key::Home, Key::End, Key::Home, Key::End]);
        assert_eq!(decode(b"\x1b[1~\x1b[4~\x1b[3~"), [Key::Home, Key::End, Key::Delete]);
        assert_eq!(decode(b"\x1b[1;5C"), [Key::Right]); // Ctrl-Right: modifiers ignored
        assert_eq!(decode(b"\x1b[15~x"), [Key::Char('x')]); // F5: unknown, dropped
        assert_eq!(decode(b"\x1bx"), [Key::Escape, Key::Char('x')]); // not a sequence
        assert_eq!(decode(b"\x1b[999999999A"), []); // too long: dropped
    }

    #[test]
    fn serial_lone_escape_after_timeout() {
        let mut d = SerialDecoder::new();
        let mut out = VecDeque::new();
        d.feed(ESC, 100, &mut out);
        d.on_tick(100 + ESC_TIMEOUT_MS - 1, &mut out);
        assert!(out.is_empty()); // could still be the start of a sequence
        d.on_tick(100 + ESC_TIMEOUT_MS, &mut out);
        assert_eq!(out.drain(..).collect::<Vec<_>>(), [Key::Escape]);
        d.feed(b'[', 200, &mut out); // the sequence state was reset
        assert_eq!(out.drain(..).collect::<Vec<_>>(), [Key::Char('[')]);
    }

    #[test]
    fn serial_utf8() {
        assert_eq!(decode("grüße €".as_bytes()), "grüße €".chars().map(Key::Char).collect::<Vec<_>>());
        assert_eq!(decode("😀".as_bytes()), [Key::Char('😀')]);
        assert_eq!(decode(b"\xc3a"), [Key::Char('a')]); // broken character dropped
        assert_eq!(decode(b"\xbc"), []); // stray continuation byte
    }
}
