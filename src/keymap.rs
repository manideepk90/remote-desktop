//! Turns VNC keysyms into physical key presses using the compositor's keymap.
//!
//! VNC clients send keysyms ("A", "exclam", "Return"). Injecting those as
//! keysyms breaks when a key's press and release arrive as different symbols
//! (press "A" with Shift held, release "a" after Shift): the compositor never
//! sees a matching release and apps auto-repeat the key forever. Mapping every
//! keysym to the physical key that produces it, and remembering which key each
//! press used, makes releases always match.

use std::collections::HashMap;

use xkbcommon::xkb;

pub const KEY_LEFTSHIFT: u32 = 42;
pub const KEY_RIGHTALT: u32 = 100;
const XK_SHIFT_L: u32 = 0xffe1;
const XK_SHIFT_R: u32 = 0xffe2;
const XK_ISO_LEVEL3_SHIFT: u32 = 0xfe03;
const XK_MODE_SWITCH: u32 = 0xff7e;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct KeyPos {
    /// Linux evdev key code.
    pub code: u32,
    pub shift: bool,
    pub altgr: bool,
}

/// keysym -> the simplest physical key combination that types it (first layout).
pub struct KeyTable {
    map: HashMap<u32, KeyPos>,
    pub shift_code: u32,
    pub altgr_code: u32,
}

impl KeyTable {
    pub fn from_keymap_string(text: String) -> Option<KeyTable> {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let keymap = xkb::Keymap::new_from_string(&ctx, text, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::KEYMAP_COMPILE_NO_FLAGS)?;
        let shift_mask = 1u32 << keymap.mod_get_index(xkb::MOD_NAME_SHIFT);
        let altgr_mask = 1u32 << keymap.mod_get_index(xkb::MOD_NAME_ISO_LEVEL3_SHIFT);

        // (number of modifiers needed, keycode) decides which position wins.
        let mut best: HashMap<u32, (u32, KeyPos)> = HashMap::new();
        let (min, max) = (keymap.min_keycode().raw(), keymap.max_keycode().raw());
        for kc in min.max(8)..=max {
            let key = xkb::Keycode::new(kc);
            if keymap.num_layouts_for_key(key) == 0 {
                continue;
            }
            for level in 0..keymap.num_levels_for_key(key, 0) {
                let mut masks = [0u32; 8];
                let n = keymap.key_get_mods_for_level(key, 0, level, &mut masks);
                // Use the simplest modifier set we know how to press.
                let Some(mask) = masks[..n].iter().copied().filter(|m| m & !(shift_mask | altgr_mask) == 0).min_by_key(|m| m.count_ones())
                else {
                    continue;
                };
                let pos = KeyPos { code: kc - 8, shift: mask & shift_mask != 0, altgr: mask & altgr_mask != 0 };
                let cost = mask.count_ones();
                for sym in keymap.key_get_syms_by_level(key, 0, level) {
                    let e = best.entry(sym.raw()).or_insert((u32::MAX, pos));
                    if cost < e.0 || (cost == e.0 && pos.code < e.1.code) {
                        *e = (cost, pos);
                    }
                }
            }
        }
        let map: HashMap<u32, KeyPos> = best.into_iter().map(|(k, (_, p))| (k, p)).collect();
        let shift_code = map.get(&XK_SHIFT_L).map_or(KEY_LEFTSHIFT, |p| p.code);
        let altgr_code = map.get(&XK_ISO_LEVEL3_SHIFT).map_or(KEY_RIGHTALT, |p| p.code);
        log::info!("keyboard layout loaded ({} keysyms)", map.len());
        Some(KeyTable { map, shift_code, altgr_code })
    }

    pub fn lookup(&self, keysym: u32) -> Option<KeyPos> {
        self.map.get(&keysym).copied()
    }
}

/// What the desktop must do for one VNC key event.
#[derive(Debug, PartialEq)]
pub enum Action {
    Key { code: u32, pressed: bool },
    /// Keysym the layout can't type: let KWin synthesize a press + release.
    Tap(u32),
}

struct Held {
    code: u32,
    /// Modifiers we pressed ourselves because the client didn't (phone keyboards).
    synthetic: Vec<u32>,
}

/// Per-session keyboard state.
#[derive(Default)]
pub struct Keyboard {
    held: HashMap<u32, Held>,
}

impl Keyboard {
    fn client_holds(&self, syms: &[u32]) -> bool {
        syms.iter().any(|s| self.held.contains_key(s))
    }

    pub fn event(&mut self, table: Option<&KeyTable>, keysym: u32, down: bool) -> Vec<Action> {
        let mut out = Vec::new();
        if down {
            if self.held.contains_key(&keysym) {
                return out; // client-side auto-repeat; the apps repeat held keys themselves
            }
            let Some(pos) = table.and_then(|t| t.lookup(keysym)) else {
                out.push(Action::Tap(keysym));
                return out;
            };
            let table = table.unwrap();
            let mut synthetic = Vec::new();
            if pos.shift && !self.client_holds(&[XK_SHIFT_L, XK_SHIFT_R]) {
                synthetic.push(table.shift_code);
            }
            if pos.altgr && !self.client_holds(&[XK_ISO_LEVEL3_SHIFT, XK_MODE_SWITCH]) {
                synthetic.push(table.altgr_code);
            }
            // Another held keysym may already be using this physical key (e.g. "a" then "A").
            if let Some(prev) = self.held.iter().find(|(_, h)| h.code == pos.code).map(|(k, _)| *k) {
                out.extend(self.release(prev));
            }
            out.extend(synthetic.iter().map(|&code| Action::Key { code, pressed: true }));
            out.push(Action::Key { code: pos.code, pressed: true });
            self.held.insert(keysym, Held { code: pos.code, synthetic });
        } else if self.held.contains_key(&keysym) {
            out.extend(self.release(keysym));
        } else if let Some(pos) = table.and_then(|t| t.lookup(keysym)) {
            // Release arrived under a different keysym than the press ("A" down, "a" up).
            if let Some(prev) = self.held.iter().find(|(_, h)| h.code == pos.code).map(|(k, _)| *k) {
                out.extend(self.release(prev));
            }
        }
        out
    }

    fn release(&mut self, keysym: u32) -> Vec<Action> {
        let Some(h) = self.held.remove(&keysym) else { return vec![] };
        let mut out = vec![Action::Key { code: h.code, pressed: false }];
        out.extend(h.synthetic.iter().rev().map(|&code| Action::Key { code, pressed: false }));
        out
    }

    /// Releases everything, e.g. when the client disconnects.
    pub fn release_all(&mut self) -> Vec<Action> {
        let keys: Vec<u32> = self.held.keys().copied().collect();
        keys.into_iter().flat_map(|k| self.release(k)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY_A: u32 = 30;
    const KEY_1: u32 = 2;

    fn us_table() -> KeyTable {
        let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
        let km = xkb::Keymap::new_from_names(&ctx, "evdev", "pc105", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS)
            .expect("us keymap (needs xkeyboard-config)");
        KeyTable::from_keymap_string(km.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1)).unwrap()
    }

    fn key(code: u32, pressed: bool) -> Action {
        Action::Key { code, pressed }
    }

    #[test]
    fn maps_us_layout() {
        let t = us_table();
        assert_eq!(t.lookup('a' as u32), Some(KeyPos { code: KEY_A, shift: false, altgr: false }));
        assert_eq!(t.lookup('A' as u32), Some(KeyPos { code: KEY_A, shift: true, altgr: false }));
        assert_eq!(t.lookup('!' as u32), Some(KeyPos { code: KEY_1, shift: true, altgr: false }));
        assert_eq!(t.lookup(0xff0d).map(|p| p.code), Some(28)); // Return
        assert_eq!(t.shift_code, KEY_LEFTSHIFT);
    }

    #[test]
    fn mismatched_release_does_not_stick() {
        let t = us_table();
        let mut k = Keyboard::default();
        assert_eq!(k.event(Some(&t), XK_SHIFT_L, true), vec![key(KEY_LEFTSHIFT, true)]);
        assert_eq!(k.event(Some(&t), 'A' as u32, true), vec![key(KEY_A, true)]);
        assert_eq!(k.event(Some(&t), XK_SHIFT_L, false), vec![key(KEY_LEFTSHIFT, false)]);
        // The release comes as lowercase: it must still release KEY_A.
        assert_eq!(k.event(Some(&t), 'a' as u32, false), vec![key(KEY_A, false)]);
        assert!(k.release_all().is_empty());
    }

    #[test]
    fn phone_keyboard_gets_synthetic_shift() {
        let t = us_table();
        let mut k = Keyboard::default();
        assert_eq!(k.event(Some(&t), 'A' as u32, true), vec![key(KEY_LEFTSHIFT, true), key(KEY_A, true)]);
        assert_eq!(k.event(Some(&t), 'A' as u32, false), vec![key(KEY_A, false), key(KEY_LEFTSHIFT, false)]);
    }

    #[test]
    fn autorepeat_and_disconnect() {
        let t = us_table();
        let mut k = Keyboard::default();
        assert_eq!(k.event(Some(&t), 'x' as u32, true).len(), 1);
        assert!(k.event(Some(&t), 'x' as u32, true).is_empty(), "repeat press ignored");
        assert_eq!(k.release_all(), vec![key(45, false)]);
    }

    #[test]
    fn unknown_keysym_is_tapped() {
        let t = us_table();
        let mut k = Keyboard::default();
        assert_eq!(k.event(Some(&t), 0xe9, true), vec![Action::Tap(0xe9)]); // eacute: not on a US keyboard
        assert!(k.event(Some(&t), 0xe9, false).is_empty());
    }
}
