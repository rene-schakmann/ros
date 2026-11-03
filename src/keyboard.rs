//! Turns USB keyboard reports into key presses.
//!
//! A HID "boot protocol" report is 8 bytes: [modifiers, reserved, key1..key6].
//! Keys are "usage IDs" (positions on the keyboard, not characters), so the
//! layout table decides which character a key produces. The report lists keys
//! that are *currently held*; a key press is a key that wasn't there before.
//!
//! Reference: USB HID Usage Tables 1.5, chapter 10 ("Keyboard/Keypad Page").

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
const SHIFT: u8 = 0x02 | 0x20; // left or right shift
const ALT_GR: u8 = 0x40; // right alt

/// Hold a key this long before it starts repeating...
const REPEAT_DELAY_MS: u64 = 500;
/// ...then repeat it at this interval.
const REPEAT_RATE_MS: u64 = 40;

/// Usage ID reported in all six key slots when too many keys are pressed.
const ERROR_ROLL_OVER: u8 = 0x01;

/// The running USB controllers (set up by `kernel_main`). A Raspberry Pi 5
/// has two, each serving one USB 2 and one USB 3 port.
pub static USB: Mutex<Vec<Xhci>> = Mutex::new(Vec::new());

#[derive(Debug)]
struct State {
    layout: Layout,
    held: [u8; 6],
    /// Key being held down for auto-repeat: (usage, key, next repeat time).
    repeat: Option<(u8, Key, u64)>,
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
    for usb in USB.lock().iter_mut() {
        while let Some(report) = usb.poll() {
            state.on_report(&report);
        }
    }
    // Serial input as a fallback, e.g. when no USB keyboard was found.
    while let Some(byte) = serial::read_byte() {
        state.queue.push_back(serial_key(byte));
    }
    state.on_tick(uptime_ms());
    state.queue.pop_front()
}

/// Translate a byte from the serial terminal.
fn serial_key(byte: u8) -> Key {
    match byte {
        b'\r' | b'\n' => Key::Enter,
        0x08 | 0x7f => Key::Backspace,
        b'\t' => Key::Tab,
        b => Key::Char(b as char),
    }
}

impl State {
    const fn new() -> Self {
        State { layout: Layout::Us, held: [0; 6], repeat: None, queue: VecDeque::new() }
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

/// Turn a key position (usage ID) plus modifiers into a [`Key`].
fn translate(layout: Layout, usage: u8, modifiers: u8) -> Option<Key> {
    let shift = modifiers & SHIFT != 0;
    let alt_gr = modifiers & ALT_GR != 0;
    let key = match usage {
        0x28 | 0x58 => Key::Enter,
        0x29 => Key::Escape,
        0x2a => Key::Backspace,
        0x2b => Key::Tab,
        0x4f => Key::Right,
        0x50 => Key::Left,
        0x51 => Key::Down,
        0x52 => Key::Up,
        _ => {
            let c = match layout {
                Layout::Us => us(usage, shift),
                Layout::De => de(usage, shift, alt_gr),
            }?;
            Key::Char(c)
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
    fn serial_bytes() {
        assert_eq!(serial_key(b'\r'), Key::Enter);
        assert_eq!(serial_key(0x7f), Key::Backspace);
        assert_eq!(serial_key(b'x'), Key::Char('x'));
    }
}
