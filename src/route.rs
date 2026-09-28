//! Where input goes: to the scene, or to a program's surface (a bar, Marea,
//! a lock screen). The session feeds it what libinput says; headless, a
//! script (`PLEAMAR_HEADLESS_INPUT`) — the same road, so what is tried
//! without a screen is what happens with one.

use crate::layers::{self, ToLayers};
use crate::screen::{self, Hit, Screen};
use pleamar::scene::{Mods, ToRender};
use std::sync::mpsc::Sender;

pub struct Route {
    /// Who has the pointer: the scene, or a program's surface (layer-shell).
    pub hit: Hit,
    /// The program's surface a button was pressed on: it keeps the pointer until it is let go.
    grab: Option<u64>,
    /// A button pressed on the scene: the scene keeps the pointer, and gets
    /// the release, wherever it is let go —over a bar, over Marea—. Given to
    /// the program under it instead, a window carried by its title bar never
    /// heard it had been let go, and stayed stuck to the mouse.
    scene_held: bool,
    /// The program's surface that took the keyboard when clicked.
    key_client: Option<u64>,
    /// Keys held down that went to a binding: their release is the binding's too.
    bound: Vec<u32>,
    to_render: Sender<ToRender>,
}

impl Route {
    pub fn new(to_render: Sender<ToRender>) -> Route {
        Route { hit: Hit::Scene(None), grab: None, scene_held: false, key_client: None, bound: Vec::new(), to_render }
    }

    /// The pointer at that point of a monitor, in its pixels. Says whether it
    /// went from the scene to a program or back (the cursor changes hands).
    pub fn pointer(&mut self, on: &Screen, (mx, my): (f64, f64)) -> bool {
        let hit = {
            let st = on.0.lock().unwrap();
            // (Dragging something out of it, the pointer goes where it is:
            // the windows and surfaces it crosses are where it may be let go.)
            match self.grab.filter(|_| !layers::dragging()) {
                // Held: the one it was pressed on keeps it, wherever it goes.
                Some(id) => match st.clients.iter().find(|c| c.id == id) {
                    Some(c) => Hit::Client(id, (mx - c.rect[0] as f64, my - c.rect[1] as f64)),
                    None => self.hit,
                },
                None if self.scene_held && !layers::dragging() => match st.layers.iter().find(|l| l.main) {
                    Some(l) => Hit::Scene(Some((l.origin.0 + (mx - l.rect[0] as f64) as f32 / l.scale, l.origin.1 + (my - l.rect[1] as f64) as f32 / l.scale))),
                    None => screen::pointer_at(&st, (mx, my)),
                },
                None => screen::pointer_at(&st, (mx, my)),
            }
        };
        self.point(hit)
    }

    /// The pointer goes to whoever takes it now, and leaves whoever had it.
    fn point(&mut self, hit: Hit) -> bool {
        match hit {
            Hit::Scene(at) => {
                if matches!(self.hit, Hit::Client(..)) {
                    layers::tell(ToLayers::PointerOut);
                }
                let _ = self.to_render.send(ToRender::Pointer(at));
            }
            Hit::Client(id, (x, y)) => {
                if matches!(self.hit, Hit::Scene(Some(_))) {
                    let _ = self.to_render.send(ToRender::Pointer(None));
                }
                layers::tell(ToLayers::Pointer { id, x, y });
            }
        }
        let was_program = matches!(self.hit, Hit::Client(..));
        self.hit = hit;
        was_program != matches!(hit, Hit::Client(..))
    }

    /// A button (evdev code). Says whether a program's surface had it: then,
    /// let go, the pointer is to be placed again (the grab is over).
    pub fn button(&mut self, screens: &[Screen], code: u32, down: bool) -> bool {
        // Let go while a program drags something: straight to the compositor,
        // which drops it wherever the pointer is (a window, a surface, the scene).
        if layers::dragging() && !down {
            self.grab = None;
            layers::tell(ToLayers::Button { code, down });
            return true;
        }
        // Let go of what was pressed on the scene: the scene's, wherever it is.
        if self.scene_held && !down {
            self.scene_held = false;
            if let 0x110..=0x116 = code {
                let _ = self.to_render.send(ToRender::Button((code - 0x110) as u8, false));
            }
            return false;
        }
        if let Hit::Client(id, _) = self.hit {
            if down {
                self.grab = Some(id);
                let takes = screens.iter().any(|s| screen::takes_keyboard_on_click(&s.0.lock().unwrap(), id));
                self.set_key_client(if takes { Some(id) } else { None });
            } else {
                self.grab = None;
            }
            layers::tell(ToLayers::Button { code, down });
            return true;
        }
        if down {
            self.set_key_client(None);
            layers::tell(ToLayers::ScenePress);
            self.scene_held = true;
        }
        // Left, right, middle; and the side ones (back, forward), which only
        // the windows use.
        let b = match code {
            0x110..=0x116 => (code - 0x110) as u8,
            _ => return false,
        };
        let _ = self.to_render.send(ToRender::Button(b, down));
        false
    }

    /// The wheel, in notches: to whoever has the pointer.
    pub fn wheel(&self, notches: f32) {
        if matches!(self.hit, Hit::Client(..)) {
            layers::tell(ToLayers::Wheel(notches));
        } else {
            let _ = self.to_render.send(ToRender::Wheel(notches));
        }
    }

    /// Where the keys go: a program's surface that takes all of it; else the
    /// one clicked that takes it on demand; else the scene.
    pub fn key_owner(&self, screens: &[Screen]) -> Option<u64> {
        let all = screens.iter().find_map(|s| screen::keyboard_taker(&s.0.lock().unwrap()));
        // Only while it still asks for it: a card that closes gives it back.
        let alive = |id: u64| screens.iter().any(|s| s.0.lock().unwrap().clients.iter().any(|c| c.id == id && c.keyboard != 0 && !c.pieces.is_empty()));
        all.or(self.key_client.filter(|id| alive(*id)))
    }

    /// A key, by its keysym's name, what it types (if anything) and its evdev code.
    /// `base`: the key's own name, without what Shift makes of it (`1` where
    /// it types `!`): a binding says `Super+Shift+1`, whatever the layout.
    pub fn key(&mut self, screens: &[Screen], name: &str, base: Option<&str>, typed: Option<String>, mods: Mods, evdev: u32, down: bool) {
        // A binding (keys.conf) takes the key before anyone —not while locked—.
        if down && !layers::locked() {
            let keys = crate::keys::get();
            if let Some(action) = keys.find(name, mods).or_else(|| base.and_then(|b| keys.find(b, mods))) {
                println!("session · key {}{}{}{}{name} → {action:?}", if mods.ctrl { "Ctrl+" } else { "" }, if mods.alt { "Alt+" } else { "" }, if mods.shift { "Shift+" } else { "" }, if mods.logo { "Super+" } else { "" });
                self.perform(action);
                self.bound.push(evdev);
                return;
            }
        }
        if !down {
            if let Some(k) = self.bound.iter().position(|c| *c == evdev) {
                self.bound.remove(k);
                return;
            }
        }
        if down {
            let owner = self.key_owner(screens);
            // Locked, a key goes to the lock screen or nowhere: never to the
            // scene or its windows behind it.
            if owner.is_none() && layers::locked() {
                return;
            }
            // Shortcuts (never what is typed): where each went, to find out why one does nothing.
            if mods.ctrl || mods.alt || mods.logo || name == "Escape" {
                println!("session · key {}{}{}{name} → {}", if mods.ctrl { "Ctrl+" } else { "" }, if mods.alt { "Alt+" } else { "" }, if mods.logo { "Super+" } else { "" }, owner.map_or("the scene".to_owned(), |id| format!("the program's surface {id}")));
            }
            if let Some(id) = owner {
                layers::tell(ToLayers::Key { id, code: evdev, down: true });
                return;
            }
            let _ = self.to_render.send(ToRender::Key(name.to_owned(), typed, mods, evdev));
        } else {
            // A key let go goes where it went down; to both, if that is not known.
            if let Some(id) = self.key_owner(screens) {
                layers::tell(ToLayers::Key { id, code: evdev, down: false });
            }
            if layers::locked() {
                return;
            }
            let _ = self.to_render.send(ToRender::KeyReleased(name.to_owned(), evdev));
        }
    }

    /// A binding's action: a program, or an event of the window manager's scene.
    pub fn perform(&self, action: &crate::keys::Action) {
        match action {
            crate::keys::Action::Launch(command) => layers::tell(ToLayers::Launch(command.clone())),
            crate::keys::Action::Emit(event, n) => {
                let _ = self.to_render.send(ToRender::ExternalSignal(pleamar::scene::intern(event), *n));
            }
        }
    }

    fn set_key_client(&mut self, id: Option<u64>) {
        if self.key_client.is_some() && id.is_none() {
            layers::tell(ToLayers::KeyboardBack);
        }
        self.key_client = id;
    }
}
