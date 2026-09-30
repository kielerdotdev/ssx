//! Pure Win32 input decoding (virtual-key codes, `lParam`/`wParam` packing).
//!
//! Kept free of `windows` crate types and compiled on every platform so it is unit tested on
//! the Linux CI; the Windows backend is a thin shell around these functions.
#![cfg_attr(not(windows), allow(dead_code))] // only the Windows backend calls these

use ssx_types::Point;

use crate::model::{Key, Modifiers};

/// Virtual-key codes (`winuser.h`).
mod vk {
    pub const TAB: u32 = 0x09;
    pub const RETURN: u32 = 0x0d;
    pub const SHIFT: u32 = 0x10;
    pub const CONTROL: u32 = 0x11;
    pub const MENU: u32 = 0x12;
    pub const ESCAPE: u32 = 0x1b;
    pub const SPACE: u32 = 0x20;
    pub const LEFT: u32 = 0x25;
    pub const UP: u32 = 0x26;
    pub const RIGHT: u32 = 0x27;
    pub const DOWN: u32 = 0x28;
    pub const LSHIFT: u32 = 0xa0;
    pub const RSHIFT: u32 = 0xa1;
    pub const LCONTROL: u32 = 0xa2;
    pub const RCONTROL: u32 = 0xa3;
    pub const LMENU: u32 = 0xa4;
    pub const RMENU: u32 = 0xa5;
}

/// Translates a virtual-key code from `WM_KEYDOWN`/`WM_KEYUP`.
pub(crate) fn key_from_vk(code: u32) -> Key {
    match code {
        vk::TAB => Key::Tab,
        vk::RETURN => Key::Enter,
        vk::ESCAPE => Key::Escape,
        vk::SPACE => Key::Space,
        vk::LEFT => Key::Left,
        vk::UP => Key::Up,
        vk::RIGHT => Key::Right,
        vk::DOWN => Key::Down,
        vk::SHIFT | vk::LSHIFT | vk::RSHIFT => Key::Shift,
        vk::CONTROL | vk::LCONTROL | vk::RCONTROL => Key::Control,
        vk::MENU | vk::LMENU | vk::RMENU => Key::Alt,
        0x41..=0x5a => Key::Char((code as u8 + 32) as char),
        0x30..=0x39 => Key::Char(code as u8 as char),
        _ => Key::Other,
    }
}

/// Client-area position packed in a mouse message's `lParam` (two signed 16-bit values),
/// translated by the window origin to a desktop pixel.
pub(crate) fn point_from_lparam(lparam: isize, origin: Point) -> Point {
    let x = i32::from(lparam as u16 as i16);
    let y = i32::from((lparam >> 16) as u16 as i16);
    Point::new(origin.x.saturating_add(x), origin.y.saturating_add(y))
}

/// Signed wheel delta from `WM_MOUSEWHEEL`'s `wParam` (high word; one notch is 120, but
/// high-resolution wheels send fractions, which the model treats by sign).
pub(crate) fn wheel_delta(wparam: usize) -> i32 {
    i32::from((wparam >> 16) as u16 as i16)
}

/// Shift/Ctrl from a mouse message's `wParam` key-state flags (`MK_SHIFT`, `MK_CONTROL`);
/// Alt is not part of them and is read with `GetKeyState`.
pub(crate) fn modifiers_from_mk(wparam: usize, alt: bool) -> Modifiers {
    Modifiers { shift: wparam & 0x4 != 0, ctrl: wparam & 0x8 != 0, alt }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn virtual_keys_map() {
        assert_eq!(key_from_vk(0x1b), Key::Escape);
        assert_eq!(key_from_vk(0x0d), Key::Enter);
        assert_eq!(key_from_vk(0x09), Key::Tab);
        assert_eq!(key_from_vk(0x20), Key::Space);
        assert_eq!(key_from_vk(0x25), Key::Left);
        assert_eq!(key_from_vk(0x28), Key::Down);
        assert_eq!(key_from_vk(0xa0), Key::Shift);
        assert_eq!(key_from_vk(0xa3), Key::Control);
        assert_eq!(key_from_vk(0xa4), Key::Alt);
        assert_eq!(key_from_vk(0x43), Key::Char('c'));
        assert_eq!(key_from_vk(0x35), Key::Char('5'));
        assert_eq!(key_from_vk(0x70), Key::Other, "F1");
    }

    #[test]
    fn lparam_positions_are_signed_16_bit_and_offset_by_the_origin() {
        let pack = |x: i16, y: i16| ((y as u16 as isize) << 16) | (x as u16 as isize);
        assert_eq!(point_from_lparam(pack(10, 20), Point::new(0, 0)), Point::new(10, 20));
        assert_eq!(point_from_lparam(pack(10, 20), Point::new(-1920, 5)), Point::new(-1910, 25));
        assert_eq!(point_from_lparam(pack(-3, -4), Point::new(100, 100)), Point::new(97, 96));
        assert_eq!(
            point_from_lparam(pack(i16::MAX, i16::MIN), Point::new(0, 0)),
            Point::new(32767, -32768)
        );
    }

    #[test]
    fn wheel_and_modifier_decoding() {
        let w = |d: i16| (usize::from(d as u16)) << 16;
        assert_eq!(wheel_delta(w(120)), 120);
        assert_eq!(wheel_delta(w(-240)), -240);
        assert_eq!(wheel_delta(w(30)), 30, "high-resolution wheels report fractions of a notch");
        let m = modifiers_from_mk(0x4 | 0x8, true);
        assert!(m.shift && m.ctrl && m.alt);
        assert_eq!(modifiers_from_mk(0, false), Modifiers::default());
    }
}
