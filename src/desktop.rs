//! The installed programs, as their `.desktop` files say: to pin one to the
//! dock by its name, and to find the icon of the program a window belongs to.

use pleamar::scene::DockPin;
use std::sync::OnceLock;

struct Entry {
    /// The file's name without `.desktop`: the app id its windows usually say.
    id: String,
    name: String,
    exec: String,
    icon: String,
    /// The X11 class its windows say, if it says one.
    class: String,
}

fn entries() -> &'static Vec<Entry> {
    static ENTRIES: OnceLock<Vec<Entry>> = OnceLock::new();
    ENTRIES.get_or_init(|| {
        let home = std::env::var("HOME").unwrap_or_default();
        let data_dirs = std::env::var("XDG_DATA_DIRS").unwrap_or_else(|_| "/usr/local/share:/usr/share".into());
        let mut folders = vec![format!("{home}/.local/share/applications")];
        folders.extend(data_dirs.split(':').map(|d| format!("{d}/applications")));
        let mut out: Vec<Entry> = Vec::new();
        for folder in folders {
            let Ok(dir) = std::fs::read_dir(&folder) else { continue };
            for f in dir.filter_map(Result::ok).filter(|f| f.path().extension().is_some_and(|e| e == "desktop")) {
                let id = f.path().file_stem().and_then(|s| s.to_str()).unwrap_or("").to_owned();
                // The user's folder comes first and wins.
                if out.iter().any(|e| e.id == id) {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(f.path()) else { continue };
                let (mut name, mut exec, mut icon, mut class, mut inside) = (String::new(), String::new(), String::new(), String::new(), false);
                for l in text.lines() {
                    if l.starts_with('[') {
                        inside = l == "[Desktop Entry]";
                    } else if inside {
                        match l.split_once('=') {
                            Some(("Name", v)) if name.is_empty() => name = v.to_owned(),
                            Some(("Exec", v)) => exec = v.split_whitespace().filter(|p| !p.starts_with('%')).collect::<Vec<_>>().join(" "),
                            Some(("Icon", v)) => icon = v.to_owned(),
                            Some(("StartupWMClass", v)) => class = v.to_owned(),
                            _ => {}
                        }
                    }
                }
                if !exec.is_empty() {
                    out.push(Entry { id, name, exec, icon, class });
                }
            }
        }
        out
    })
}

/// The entry a name means: its file (`org.telegram.desktop`, or its last
/// part, `telegram`), the class its windows say, or its name.
fn find(word: &str) -> Option<&'static Entry> {
    let w = word.to_lowercase();
    let e = entries();
    e.iter()
        .find(|x| x.id.to_lowercase() == w)
        .or_else(|| e.iter().find(|x| x.class.to_lowercase() == w))
        .or_else(|| e.iter().find(|x| x.id.to_lowercase().rsplit('.').next() == Some(w.as_str())))
        .or_else(|| e.iter().find(|x| x.name.to_lowercase() == w))
}

/// A program pinned to the dock by a word of `dock …` in `session.conf`: the
/// names its windows may go by, its icon and how it starts. Not installed,
/// the word itself is all three.
pub fn pin(word: &str) -> DockPin {
    let w = word.to_lowercase();
    match find(word) {
        Some(e) => {
            let mut keys = vec![w.clone(), e.id.to_lowercase()];
            if !e.class.is_empty() {
                keys.push(e.class.to_lowercase());
            }
            // The program it runs, too: `kitty`'s windows say `kitty`.
            if let Some(bin) = e.exec.split_whitespace().next().and_then(|b| b.rsplit('/').next()) {
                keys.push(bin.to_lowercase());
            }
            keys.dedup();
            DockPin { keys, icon: if e.icon.is_empty() { w } else { e.icon.clone() }, exec: e.exec.clone() }
        }
        None => DockPin { keys: vec![w.clone()], icon: w.clone(), exec: word.to_owned() },
    }
}

/// The icon of the program whose windows say that app id (or class).
/// Nothing installed says it: the generic one, not an empty space.
pub fn icon_for(app: &str) -> String {
    match find(app) {
        Some(e) if !e.icon.is_empty() => e.icon.clone(),
        _ => "application-x-executable".to_owned(),
    }
}
