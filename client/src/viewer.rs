//! The remote screen: draws the framebuffer and turns local keyboard and mouse
//! input into RFB events.

use std::collections::HashMap;

use egui::{
    Color32, ColorImage, Event, Key, Modifiers, MouseWheelUnit, PointerButton, Pos2, Rect, Sense, TextureHandle,
    TextureOptions, Ui, Vec2, pos2, vec2,
};

use crate::keys;
use crate::profiles::Scale;
use crate::rfb::{Session, button};

/// Things the viewer asks the app to do in response to a hotkey.
#[derive(Debug, PartialEq)]
pub enum Action {
    ToggleFullscreen,
}

#[derive(Default)]
struct InputState {
    /// Buttons currently held on the remote side.
    buttons: u8,
    /// Keys pressed on the remote side, by physical key, so a release always
    /// matches its press even if the layout or modifiers changed in between.
    held: HashMap<Key, u32>,
    modifiers: Modifiers,
    /// Last position sent, in remote pixels.
    last: Option<(u16, u16)>,
    /// Unsent scroll, in wheel clicks.
    wheel: Vec2,
}

pub struct Viewer {
    pub scale: Scale,
    pub view_only: bool,
    texture: Option<TextureHandle>,
    filter: TextureOptions,
    cursor: Option<(TextureHandle, Vec2, Vec2)>,
    cursor_serial: u64,
    size: (usize, usize),
    /// Where the screen was drawn last frame, and the part of it that was visible.
    image_rect: Option<Rect>,
    visible_rect: Option<Rect>,
    input: InputState,
}

impl Viewer {
    pub fn new(scale: Scale, view_only: bool) -> Viewer {
        Viewer {
            scale,
            view_only,
            texture: None,
            filter: TextureOptions::LINEAR,
            cursor: None,
            cursor_serial: 0,
            size: (0, 0),
            image_rect: None,
            visible_rect: None,
            input: InputState::default(),
        }
    }

    pub fn has_image(&self) -> bool {
        self.texture.is_some()
    }

    // ---- input -------------------------------------------------------------

    /// Takes keyboard events out of `raw` (so egui never acts on them) and
    /// forwards keyboard and mouse input to the server. `blocked` is screen area
    /// covered by viewer controls, where clicks belong to egui.
    pub fn handle_input(&mut self, raw: &mut egui::RawInput, session: &Session, blocked: Option<Rect>) -> Vec<Action> {
        let mut actions = Vec::new();
        let mut keep = Vec::with_capacity(raw.events.len());
        for event in std::mem::take(&mut raw.events) {
            match &event {
                Event::Key { .. } | Event::Text(_) | Event::Copy | Event::Cut | Event::Paste(_) => {
                    self.keyboard(&event, session, &mut actions);
                }
                Event::ModifiersChanged(m) => {
                    self.input.modifiers = *m;
                    keep.push(event);
                }
                Event::PointerMoved(pos) => {
                    self.pointer_moved(*pos, session, blocked);
                    keep.push(event);
                }
                Event::PointerButton { pos, button, pressed, .. } => {
                    self.pointer_button(*pos, *button, *pressed, session, blocked);
                    keep.push(event);
                }
                Event::MouseWheel { unit, delta, .. } => {
                    let over = raw_pointer_pos(&keep).is_some_and(|p| self.over_screen(p, blocked))
                        || self.input.last.is_some();
                    if over && !self.view_only {
                        self.wheel(*unit, *delta, session);
                    } else {
                        keep.push(event);
                    }
                }
                Event::WindowFocused(false) => {
                    self.release_all(session);
                    keep.push(event);
                }
                Event::PointerGone => {
                    self.input.last = None;
                    keep.push(event);
                }
                _ => keep.push(event),
            }
        }
        raw.events = keep;
        actions
    }

    fn keyboard(&mut self, event: &Event, session: &Session, actions: &mut Vec<Action>) {
        match event {
            Event::Key { key, physical_key, pressed, modifiers, .. } => {
                let id = physical_key.unwrap_or(*key);
                if *pressed && *key == Key::Enter && modifiers.ctrl && modifiers.alt {
                    if !self.input.held.contains_key(&id) {
                        actions.push(Action::ToggleFullscreen);
                    }
                    return;
                }
                if self.view_only {
                    return;
                }
                if *pressed {
                    // egui reports auto-repeat as more presses; the remote side repeats on its own.
                    if self.input.held.contains_key(&id) {
                        return;
                    }
                    let command = modifiers.ctrl || modifiers.alt || modifiers.mac_cmd;
                    if keys::is_text_key(*key) && !command {
                        return; // sent from the Text event, which follows the local layout
                    }
                    if let Some(sym) = keys::keysym(*key, modifiers.shift) {
                        self.input.held.insert(id, sym);
                        session.key(sym, true);
                    }
                } else if let Some(sym) = self.input.held.remove(&id) {
                    session.key(sym, false);
                }
            }
            Event::Text(text) => {
                let m = self.input.modifiers;
                if self.view_only || m.ctrl || m.alt || m.mac_cmd {
                    return;
                }
                for c in text.chars() {
                    let sym = keys::char_keysym(c);
                    session.key(sym, true);
                    session.key(sym, false);
                }
            }
            // egui turns these shortcuts into clipboard events; replay them as keys.
            Event::Copy => self.shortcut(session, 'c', keys::INSERT),
            Event::Cut => self.shortcut(session, 'x', keys::DELETE),
            Event::Paste(text) => {
                if !self.view_only {
                    session.clipboard(text);
                }
                self.shortcut(session, 'v', keys::INSERT);
            }
            _ => {}
        }
    }

    /// Ctrl+C / Ctrl+X / Ctrl+V, or their Windows Insert/Delete forms. The
    /// modifiers are already down on the remote side.
    fn shortcut(&self, session: &Session, letter: char, alternative: u32) {
        if self.view_only {
            return;
        }
        let m = self.input.modifiers;
        let sym = if m.command || m.ctrl { letter as u32 } else { alternative };
        session.key(sym, true);
        session.key(sym, false);
    }

    fn over_screen(&self, pos: Pos2, blocked: Option<Rect>) -> bool {
        self.visible_rect.is_some_and(|r| r.contains(pos)) && !blocked.is_some_and(|b| b.contains(pos))
    }

    fn to_remote(&self, pos: Pos2) -> Option<(u16, u16)> {
        let r = self.image_rect?;
        let (w, h) = self.size;
        if w == 0 || h == 0 {
            return None;
        }
        let x = ((pos.x - r.min.x) / r.width() * w as f32).floor().clamp(0.0, (w - 1) as f32);
        let y = ((pos.y - r.min.y) / r.height() * h as f32).floor().clamp(0.0, (h - 1) as f32);
        Some((x as u16, y as u16))
    }

    fn send_pointer(&mut self, pos: Pos2, session: &Session, force: bool) {
        let Some(p) = self.to_remote(pos) else { return };
        if force || self.input.last != Some(p) {
            session.pointer(self.input.buttons, p.0, p.1);
            self.input.last = Some(p);
        }
    }

    fn pointer_moved(&mut self, pos: Pos2, session: &Session, blocked: Option<Rect>) {
        if self.view_only {
            return;
        }
        // Keep following a drag that leaves the screen, clamped to its edge.
        if self.over_screen(pos, blocked) || self.input.buttons != 0 {
            self.send_pointer(pos, session, false);
        } else {
            self.input.last = None;
        }
    }

    fn pointer_button(&mut self, pos: Pos2, b: PointerButton, pressed: bool, session: &Session, blocked: Option<Rect>) {
        if self.view_only {
            return;
        }
        let bit = match b {
            PointerButton::Primary => button::LEFT,
            PointerButton::Middle => button::MIDDLE,
            PointerButton::Secondary => button::RIGHT,
            PointerButton::Extra1 => button::BACK,
            PointerButton::Extra2 => return, // forward has no bit in the 8-bit RFB mask
        };
        if pressed {
            if !self.over_screen(pos, blocked) {
                return;
            }
            self.input.buttons |= bit;
        } else {
            if self.input.buttons & bit == 0 {
                return;
            }
            self.input.buttons &= !bit;
        }
        self.send_pointer(pos, session, true);
    }

    fn wheel(&mut self, unit: MouseWheelUnit, delta: Vec2, session: &Session) {
        let Some((x, y)) = self.input.last else { return };
        let clicks = match unit {
            MouseWheelUnit::Line => delta,
            MouseWheelUnit::Point => delta / 50.0,
            MouseWheelUnit::Page => delta * 3.0,
        };
        self.input.wheel += clicks;
        let w = &mut self.input.wheel;
        let mut bits = Vec::new();
        for (amount, positive, negative) in
            [(&mut w.y, button::WHEEL_UP, button::WHEEL_DOWN), (&mut w.x, button::WHEEL_LEFT, button::WHEEL_RIGHT)]
        {
            while *amount >= 1.0 {
                bits.push(positive);
                *amount -= 1.0;
            }
            while *amount <= -1.0 {
                bits.push(negative);
                *amount += 1.0;
            }
        }
        for bit in bits {
            session.pointer(self.input.buttons | bit, x, y);
            session.pointer(self.input.buttons, x, y);
        }
    }

    /// Lets go of every key and button held on the remote side.
    pub fn release_all(&mut self, session: &Session) {
        for (_, sym) in self.input.held.drain() {
            session.key(sym, false);
        }
        if self.input.buttons != 0 {
            self.input.buttons = 0;
            if let Some((x, y)) = self.input.last {
                session.pointer(0, x, y);
            }
        }
        self.input.wheel = Vec2::ZERO;
    }

    pub fn send_ctrl_alt_del(&self, session: &Session) {
        for sym in [keys::CONTROL_L, keys::ALT_L, keys::DELETE] {
            session.key(sym, true);
        }
        for sym in [keys::DELETE, keys::ALT_L, keys::CONTROL_L] {
            session.key(sym, false);
        }
    }

    // ---- drawing -----------------------------------------------------------

    fn sync_texture(&mut self, ctx: &egui::Context, session: &Session, filter: TextureOptions) {
        let mut fb = session.fb.lock().unwrap();
        if fb.width == 0 || fb.height == 0 {
            return;
        }
        if fb.cursor_serial != self.cursor_serial {
            self.cursor_serial = fb.cursor_serial;
            self.cursor = fb.cursor.as_ref().map(|c| {
                let image = ColorImage::new([c.width, c.height], c.pixels.clone());
                let tex = ctx.load_texture("remote-cursor", image, TextureOptions::LINEAR);
                (tex, vec2(c.width as f32, c.height as f32), vec2(c.hotspot.0 as f32, c.hotspot.1 as f32))
            });
        }
        let full = self.texture.is_none() || fb.resized || filter != self.filter;
        if full {
            let image = ColorImage::new([fb.width, fb.height], fb.pixels.clone());
            self.size = (fb.width, fb.height);
            fb.resized = false;
            fb.dirty = None;
            drop(fb);
            self.filter = filter;
            match &mut self.texture {
                Some(t) => t.set(image, filter),
                None => self.texture = Some(ctx.load_texture("remote-screen", image, filter)),
            }
        } else if let Some(d) = fb.dirty.take() {
            let image = ColorImage::new([d.w, d.h], fb.region(d));
            drop(fb);
            if let Some(t) = &mut self.texture {
                t.set_partial([d.x, d.y], image, filter);
            }
        }
    }

    /// Draws the remote screen into the rest of `ui`.
    pub fn show(&mut self, ui: &mut Ui, session: &Session) {
        let area = ui.available_rect_before_wrap();
        let ppp = ui.ctx().pixels_per_point();
        let (w, h) = {
            let fb = session.fb.lock().unwrap();
            (fb.width, fb.height)
        };
        ui.painter().rect_filled(area, 0.0, Color32::from_gray(18));
        if w == 0 || h == 0 {
            return;
        }
        let actual = vec2(w as f32, h as f32) / ppp;
        let fit = (area.width() / actual.x).min(area.height() / actual.y);
        // Sharp pixels when drawn 1:1, smooth when scaled.
        let one_to_one = self.scale == Scale::Actual || (fit - 1.0).abs() < 0.01;
        self.sync_texture(ui.ctx(), session, if one_to_one { TextureOptions::NEAREST } else { TextureOptions::LINEAR });
        let Some(tex) = &self.texture else { return };
        let uv = Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0));

        match self.scale {
            Scale::Fit => {
                let size = if one_to_one { actual } else { actual * fit };
                let min = snap(area.center() - size / 2.0, ppp);
                let rect = Rect::from_min_size(min, size);
                ui.painter().image(tex.id(), rect, uv, Color32::WHITE);
                self.image_rect = Some(rect);
                self.visible_rect = Some(rect.intersect(area));
                ui.allocate_rect(area, Sense::hover());
            }
            Scale::Actual => {
                let tex_id = tex.id();
                let out = egui::ScrollArea::both()
                    .scroll_source(egui::scroll_area::ScrollSource::SCROLL_BAR)
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        let room = ui.available_size();
                        // Centre a screen smaller than the window.
                        let pad = ((room - actual) / 2.0).max(Vec2::ZERO);
                        let (outer, _) = ui.allocate_exact_size(actual + pad * 2.0, Sense::hover());
                        let rect = Rect::from_min_size(snap(outer.min + pad, ppp), actual);
                        ui.painter().image(tex_id, rect, uv, Color32::WHITE);
                        rect
                    });
                self.image_rect = Some(out.inner);
                self.visible_rect = Some(out.inner.intersect(out.inner_rect));
            }
        }
        self.draw_cursor(ui);
    }

    /// Draws the server's cursor shape at the local pointer, or a small dot when
    /// the server draws the cursor into the image itself.
    fn draw_cursor(&self, ui: &Ui) {
        if self.view_only {
            return;
        }
        let (Some(pos), Some(image), Some(visible)) =
            (ui.ctx().pointer_hover_pos(), self.image_rect, self.visible_rect)
        else {
            return;
        };
        if !visible.contains(pos) || !ui.ui_contains_pointer() {
            return;
        }
        ui.ctx().set_cursor_icon(egui::CursorIcon::None);
        let painter = ui.ctx().layer_painter(egui::LayerId::new(egui::Order::Tooltip, egui::Id::new("remote-cursor")));
        match &self.cursor {
            Some((tex, size, hotspot)) => {
                let scale = image.width() / self.size.0.max(1) as f32;
                let rect = Rect::from_min_size(pos - *hotspot * scale, *size * scale);
                painter.image(tex.id(), rect, Rect::from_min_max(pos2(0.0, 0.0), pos2(1.0, 1.0)), Color32::WHITE);
            }
            None => {
                painter.circle(pos, 2.5, Color32::WHITE, egui::Stroke::new(1.0, Color32::BLACK));
            }
        }
    }
}

/// Rounds a position to whole physical pixels.
fn snap(p: Pos2, ppp: f32) -> Pos2 {
    pos2((p.x * ppp).round() / ppp, (p.y * ppp).round() / ppp)
}

/// The latest pointer position among events already processed this frame.
fn raw_pointer_pos(events: &[Event]) -> Option<Pos2> {
    events.iter().rev().find_map(|e| match e {
        Event::PointerMoved(p) => Some(*p),
        _ => None,
    })
}
