//! The session's key bindings: `~/.config/pleamar/keys.conf`, or the ones
//! that come with pleamar-wm (`keys.conf` here) if there is none. One a line:
//!
//! ```text
//! defaults                            the ones that come with it, here; then add or change
//! bind Super+Return launch kitty      a program
//! bind Super+q close                  an action of the window manager's scene (its events)
//! unbind Super+t                      one of the defaults, gone
//! bind Super+3 workspace 3            an action with a number (its payload)
//! gesture swipe3_down close           a touchpad gesture, the same way
//! bind XF86AudioRaiseVolume repeat locked launch marea volume_up
//! ```
//!
//! Before the action, `repeat` makes a held key act again at the keyboard's
//! pace (volume, brightness), `locked` lets it act with the screen locked, and
//! `release` makes it act when the key is let go —hold to talk, beside the
//! same key's binding that starts it—.
//!
//! An action is any event the window manager's scene declares, so a scene of
//! one's own (`~/.config/pleamar/wm/session.plm`) brings its own actions and
//! this file names them. A bound key goes to the binding and to nobody else:
//! not to the scene, not to a program.

use pleamar::scene::Mods;
use std::sync::OnceLock;

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Launch(String),
    /// An event of the scene, with a number if the line gives one:
    /// `bind Super+3 workspace 3`.
    Emit(String, Option<f32>),
}

#[derive(Clone, Debug)]
pub struct Bind {
    ctrl: bool,
    alt: bool,
    shift: bool,
    logo: bool,
    key: String,
    pub action: Action,
    /// Held down, it acts again at the keyboard's pace.
    pub repeat: bool,
    /// It acts with the screen locked too.
    pub locked: bool,
    /// It acts when the key is let go, not when it goes down.
    pub release: bool,
}

impl Bind {
    fn same(&self, o: &Bind) -> bool {
        o.key == self.key && o.ctrl == self.ctrl && o.alt == self.alt && o.shift == self.shift && o.logo == self.logo && o.release == self.release
    }
}

#[derive(Default, Debug)]
pub struct Keys {
    binds: Vec<Bind>,
    gestures: Vec<(String, Action)>,
}

impl Keys {
    /// What a key does, if it is bound: by its keysym's name, with the
    /// modifiers held. Its action, and how it acts.
    pub fn bind(&self, name: &str, mods: Mods) -> Option<&Bind> {
        self.find(name, mods, false)
    }

    /// What the same key does when it is let go (`release`).
    pub fn on_release(&self, name: &str, mods: Mods) -> Option<&Bind> {
        self.find(name, mods, true)
    }

    fn find(&self, name: &str, mods: Mods, release: bool) -> Option<&Bind> {
        let name = name.to_lowercase();
        self.binds.iter().rev().find(|b| b.key == name && b.ctrl == mods.ctrl && b.alt == mods.alt && b.shift == mods.shift && b.logo == mods.logo && b.release == release)
    }

    pub fn gesture(&self, name: &str) -> Option<&Action> {
        self.gestures.iter().rev().find(|(g, _)| g == name).map(|(_, a)| a)
    }
}

/// The bindings that come with pleamar-wm.
pub const DEFAULTS: &str = include_str!("../keys.conf");

pub fn get() -> &'static Keys {
    static KEYS: OnceLock<Keys> = OnceLock::new();
    KEYS.get_or_init(|| {
        let mut k = Keys::default();
        match crate::config::user_dir().map(|d| format!("{d}/keys.conf")).filter(|p| std::path::Path::new(p).exists()) {
            Some(file) => {
                println!("keys · {file}");
                parse(&std::fs::read_to_string(&file).unwrap_or_default(), &mut k);
            }
            None => parse(DEFAULTS, &mut k),
        }
        k
    })
}

pub fn parse(text: &str, k: &mut Keys) {
    for (n, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut words = line.splitn(3, char::is_whitespace);
        let (what, first, rest) = (words.next().unwrap_or(""), words.next().unwrap_or("").trim(), words.next().unwrap_or("").trim());
        match what {
            "defaults" => parse(DEFAULTS, k),
            "bind" => match (combo(first), flags(rest)) {
                (Some(mut b), (repeat, locked, release, Some(a))) => {
                    b.action = a;
                    b.repeat = repeat;
                    b.locked = locked;
                    b.release = release;
                    k.binds.retain(|o| !o.same(&b));
                    k.binds.push(b);
                }
                _ => eprintln!("keys · line {}: «{line}» is not «bind Mods+key action»", n + 1),
            },
            "unbind" => match combo(first) {
                //  Both: what it does going down and letting go.
                Some(b) => k.binds.retain(|o| !(o.key == b.key && o.ctrl == b.ctrl && o.alt == b.alt && o.shift == b.shift && o.logo == b.logo)),
                None => eprintln!("keys · line {}: «{line}» is not «unbind Mods+key»", n + 1),
            },
            "gesture" => match action(rest) {
                Some(a) if !first.is_empty() => {
                    k.gestures.retain(|(g, _)| g != first);
                    k.gestures.push((first.to_owned(), a));
                }
                _ => eprintln!("keys · line {}: «{line}» is not «gesture name action»", n + 1),
            },
            _ => eprintln!("keys · line {}: I don't know «{what}» (bind, unbind, gesture, defaults)", n + 1),
        }
    }
}

/// `Super+Shift+Left`: the modifiers in any order, and the key last, by its
/// keysym's name (`q`, `Return`, `Left`, `space`, `Print`, `minus`).
fn combo(s: &str) -> Option<Bind> {
    let parts: Vec<&str> = s.split('+').filter(|p| !p.is_empty()).collect();
    let (key, mods) = parts.split_last()?;
    let mut b = Bind { ctrl: false, alt: false, shift: false, logo: false, key: key.to_lowercase(), action: Action::Emit(String::new(), None), repeat: false, locked: false, release: false };
    for m in mods {
        match m.to_lowercase().as_str() {
            "super" | "logo" | "mod4" | "win" => b.logo = true,
            "ctrl" | "control" => b.ctrl = true,
            "alt" | "mod1" => b.alt = true,
            "shift" => b.shift = true,
            _ => return None,
        }
    }
    Some(b)
}

/// `repeat`, `locked` and `release` before the action, in any order.
fn flags(mut s: &str) -> (bool, bool, bool, Option<Action>) {
    let (mut repeat, mut locked, mut release) = (false, false, false);
    loop {
        s = s.trim_start();
        if let Some(r) = s.strip_prefix("repeat ") {
            repeat = true;
            s = r;
        } else if let Some(r) = s.strip_prefix("locked ") {
            locked = true;
            s = r;
        } else if let Some(r) = s.strip_prefix("release ") {
            release = true;
            s = r;
        } else {
            return (repeat, locked, release, action(s));
        }
    }
}

fn action(s: &str) -> Option<Action> {
    let s = s.trim();
    match s.split_once(char::is_whitespace) {
        Some(("launch", cmd)) if !cmd.trim().is_empty() => Some(Action::Launch(cmd.trim().to_owned())),
        None if !s.is_empty() && s != "launch" => Some(Action::Emit(s.to_owned(), None)),
        Some((event, n)) if event != "launch" => n.trim().parse::<f32>().ok().map(|n| Action::Emit(event.to_owned(), Some(n))),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binds() {
        let mut k = Keys::default();
        parse("bind Super+q close\nbind Super+Return launch kitty --single\nbind Shift+Super+m restore_last\ngesture swipe3_down close\n", &mut k);
        let sup = Mods { logo: true, ..Default::default() };
        assert_eq!(k.find("q", sup), Some(&Action::Emit("close".into(), None)));
        assert_eq!(k.find("Return", sup), Some(&Action::Launch("kitty --single".into())));
        assert_eq!(k.find("M", Mods { logo: true, shift: true, ..Default::default() }), Some(&Action::Emit("restore_last".into(), None)));
        assert_eq!(k.find("q", Mods::default()), None);
        assert_eq!(k.gesture("swipe3_down"), Some(&Action::Emit("close".into(), None)));
        parse("unbind Super+q\n", &mut k);
        assert_eq!(k.find("q", sup), None);
    }

    #[test]
    fn defaults_parse() {
        let mut k = Keys::default();
        parse(DEFAULTS, &mut k);
        assert!(k.find("q", Mods { logo: true, ..Default::default() }).is_some());
    }
}
