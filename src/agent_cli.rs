//! `pleamar-wm agent …`: the agent's hands from a shell, for an AI agent (or
//! anyone) that wants to use the desktop without writing the protocol. It
//! speaks `cua-inject v1` (see `agent.rs`) to the session's socket.
//!
//! A window is named by its process (`pleamar-wm agent windows` lists them).
//! Its coordinates are the pixels of `pleamar-wm agent look PID`: what is seen
//! in that picture at (x, y) is where `click PID x y` lands.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;

const HELP: &str = "pleamar-wm agent — use the desktop with the agent's own pointer and keyboard
(needs `agent on` in ~/.config/pleamar/session.conf; your mouse and keyboard stay yours)

  windows                         every window: process, box, seen, keyboard, program, title
  look [PID] [FILE]               a picture of that window (or of the whole desktop); prints the file
  move PID X Y                    the agent's cursor to X, Y of the window's picture
  click PID X Y [left|right|middle] [COUNT]
  drag PID X1 Y1 X2 Y2            press at one point, glide to the other, let go
  scroll PID X Y up|down|left|right [STEPS]
  type PID TEXT                   ASCII text, typed into the window without taking the keyboard
  key PID NAME                    enter tab escape backspace space up down left right delete home end pageup pagedown f1…f12
  hotkey PID MODS+KEY             ctrl+l, ctrl+shift+t, alt+f4 …
  focus PID                       give that window your keyboard (and show its workspace)
  done                            finished: the light on the monitor goes out now (by itself it
                                  waits a minute and a half, in case the agent is thinking)
  raw LINE…                       protocol lines, as they are (cua-inject v1)

Look before each click: a page moves under you.";

/// Where the session's socket is.
fn socket() -> Option<String> {
    if let Ok(p) = std::env::var("CUA_INJECT_SOCKET") {
        if !p.is_empty() {
            return Some(p);
        }
    }
    let display = std::env::var("WAYLAND_DISPLAY").ok()?;
    let path = super::agent_socket_path(&display);
    std::path::Path::new(&path).exists().then_some(path)
}

struct Hands {
    reader: BufReader<UnixStream>,
    writer: UnixStream,
}

impl Hands {
    fn open() -> Result<Self, String> {
        let path = socket().ok_or("no agent socket here: is this a pleamar-wm session with `agent on` in session.conf?")?;
        let stream = UnixStream::connect(&path).map_err(|e| format!("{path}: {e}"))?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
        let writer = stream.try_clone().map_err(|e| e.to_string())?;
        let mut hands = Hands { reader: BufReader::new(stream), writer };
        let hello = hands.say("cua-inject v1")?;
        if hello != "cua-inject v1" {
            return Err(format!("the socket answered «{hello}»"));
        }
        Ok(hands)
    }

    fn say(&mut self, line: &str) -> Result<String, String> {
        writeln!(self.writer, "{line}").map_err(|e| e.to_string())?;
        let mut reply = String::new();
        self.reader.read_line(&mut reply).map_err(|e| e.to_string())?;
        Ok(reply.trim_end().to_owned())
    }

    /// A command that has to be answered `ok`.
    fn act(&mut self, line: &str) -> Result<(), String> {
        match self.say(line)? {
            r if r == "ok" => Ok(()),
            r => Err(format!("{line}: {r}")),
        }
    }
}

fn hex(s: &str) -> String {
    s.bytes().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> String {
    let bytes: Vec<u8> = (0..s.len() / 2).filter_map(|k| u8::from_str_radix(&s[2 * k..2 * k + 2], 16).ok()).collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn button(name: Option<&String>) -> Result<u32, String> {
    match name.map(String::as_str).unwrap_or("left") {
        "left" => Ok(272),
        "right" => Ok(273),
        "middle" => Ok(274),
        other => Err(format!("a button is left, right or middle, not «{other}»")),
    }
}

fn number(s: Option<&String>, what: &str) -> Result<f64, String> {
    s.and_then(|v| v.parse::<f64>().ok()).ok_or_else(|| format!("{what}: a number"))
}

pub fn run(args: &[String]) -> i32 {
    match go(args) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("agent · {e}");
            1
        }
    }
}

fn go(args: &[String]) -> Result<(), String> {
    let Some(what) = args.first() else {
        println!("{HELP}");
        return Ok(());
    };
    let pid = || args.get(1).filter(|p| p.parse::<u32>().is_ok()).cloned().ok_or("which window: its process number (pleamar-wm agent windows)".to_owned());
    let target = |pid: &str| format!("root:{pid}");
    match what.as_str() {
        "help" | "--help" | "-h" => println!("{HELP}"),
        "windows" => {
            let reply = Hands::open()?.say("l")?;
            let list = reply.strip_prefix("windows").ok_or(reply.clone())?;
            for entry in list.split('|').map(str::trim).filter(|e| !e.is_empty()) {
                let f: Vec<&str> = entry.split_whitespace().collect();
                if f.len() < 9 {
                    continue;
                }
                let seen = if f[5] == "1" { "seen" } else { "hidden" };
                let keys = if f[6] == "1" { " · has the keyboard" } else { "" };
                println!("{:>8}  {}  «{}»  {}x{} at {},{}  {seen}{keys}", f[0], unhex(f[7]), unhex(f[8]), f[3], f[4], f[1], f[2]);
            }
        }
        "look" => {
            let out = args.get(2).cloned().unwrap_or_else(|| {
                let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
                format!("{dir}/pleamar-agent-look.png")
            });
            let shot = std::process::Command::new("grim").arg("-").output().map_err(|e| format!("grim: {e} (install grim to look)"))?;
            if !shot.status.success() {
                return Err(format!("grim: {}", String::from_utf8_lossy(&shot.stderr).trim()));
            }
            let desktop = image::load_from_memory(&shot.stdout).map_err(|e| e.to_string())?.to_rgba8();
            let picture = match args.get(1).filter(|p| p.parse::<u32>().is_ok()) {
                None => desktop,
                Some(p) => {
                    let reply = Hands::open()?.say(&format!("r {p}"))?;
                    let f: Vec<i64> = reply.strip_prefix("rect ").ok_or(reply.clone())?.split_whitespace().filter_map(|v| v.parse().ok()).collect();
                    let [x, y, w, h, seen] = f[..] else { return Err(reply) };
                    if seen == 0 || w <= 0 || h <= 0 {
                        return Err("that window is not seen now (another workspace, put away): `focus` it first".into());
                    }
                    // Cut at its box, what falls off the desktop left black: a
                    // pixel of the picture stays the point `click` takes.
                    let mut cut = image::RgbaImage::from_pixel(w as u32, h as u32, image::Rgba([0, 0, 0, 255]));
                    for py in 0..h {
                        for px in 0..w {
                            let (dx, dy) = (x + px, y + py);
                            if dx >= 0 && dy >= 0 && (dx as u32) < desktop.width() && (dy as u32) < desktop.height() {
                                cut.put_pixel(px as u32, py as u32, *desktop.get_pixel(dx as u32, dy as u32));
                            }
                        }
                    }
                    cut
                }
            };
            picture.save(&out).map_err(|e| format!("{out}: {e}"))?;
            println!("{out} {}x{}", picture.width(), picture.height());
        }
        "move" => {
            let p = pid()?;
            Hands::open()?.act(&format!("m {} 0 {} {}", target(&p), number(args.get(2), "x")?, number(args.get(3), "y")?))?;
        }
        "click" => {
            let p = pid()?;
            let (x, y) = (number(args.get(2), "x")?, number(args.get(3), "y")?);
            let b = button(args.get(4))?;
            let count = args.get(5).and_then(|c| c.parse::<u32>().ok()).unwrap_or(1).clamp(1, 3);
            let mut h = Hands::open()?;
            h.act(&format!("m {} 0 {x} {y}", target(&p)))?;
            // A moment for the cursor to be seen arriving.
            std::thread::sleep(std::time::Duration::from_millis(120));
            for _ in 0..count {
                h.act(&format!("b {} 0 {b} 1", target(&p)))?;
                h.act(&format!("b {} 0 {b} 0", target(&p)))?;
            }
        }
        "drag" => {
            let p = pid()?;
            let (x1, y1, x2, y2) = (number(args.get(2), "x1")?, number(args.get(3), "y1")?, number(args.get(4), "x2")?, number(args.get(5), "y2")?);
            let mut h = Hands::open()?;
            h.act(&format!("m {} 0 {x1} {y1}", target(&p)))?;
            h.act(&format!("b {} 0 272 1", target(&p)))?;
            for k in 1..=24 {
                let t = k as f64 / 24.0;
                h.act(&format!("m {} 0 {:.1} {:.1}", target(&p), x1 + (x2 - x1) * t, y1 + (y2 - y1) * t))?;
                std::thread::sleep(std::time::Duration::from_millis(16));
            }
            h.act(&format!("b {} 0 272 0", target(&p)))?;
        }
        "scroll" => {
            let p = pid()?;
            let (x, y) = (number(args.get(2), "x")?, number(args.get(3), "y")?);
            let (axis, value) = match args.get(4).map(String::as_str).unwrap_or("down") {
                "up" => (0, -15.0),
                "down" => (0, 15.0),
                "left" => (1, -15.0),
                "right" => (1, 15.0),
                other => return Err(format!("scroll up, down, left or right, not «{other}»")),
            };
            let steps = args.get(5).and_then(|s| s.parse::<u32>().ok()).unwrap_or(3).clamp(1, 50);
            let mut h = Hands::open()?;
            h.act(&format!("m {} 0 {x} {y}", target(&p)))?;
            for _ in 0..steps {
                h.act(&format!("a {} 0 {axis} {value}", target(&p)))?;
                std::thread::sleep(std::time::Duration::from_millis(60));
            }
        }
        "type" => {
            let p = pid()?;
            let text = args[2..].join(" ");
            if !text.is_ascii() {
                return Err("only ASCII can be typed for now (no accents or ñ)".into());
            }
            let mut h = Hands::open()?;
            // In pieces: one line of the protocol has its limits.
            for chunk in text.as_bytes().chunks(1000) {
                h.act(&format!("t {} {}", target(&p), hex(&String::from_utf8_lossy(chunk))))?;
            }
        }
        "key" => {
            let p = pid()?;
            let name = args.get(2).ok_or("which key")?;
            Hands::open()?.act(&format!("k {} {name}", target(&p)))?;
        }
        "hotkey" => {
            let p = pid()?;
            let combo = args.get(2).ok_or("which keys: ctrl+l")?;
            let mut parts: Vec<&str> = combo.split('+').collect();
            let key = parts.pop().filter(|k| !k.is_empty()).ok_or("which key, after the modifiers")?;
            if parts.is_empty() {
                Hands::open()?.act(&format!("k {} {key}", target(&p)))?;
            } else {
                Hands::open()?.act(&format!("h {} {} {key}", target(&p), parts.join(",")))?;
            }
        }
        "done" => Hands::open()?.act("x")?,
        "focus" => {
            let p = pid()?;
            Hands::open()?.act(&format!("f {p}"))?;
        }
        "raw" => {
            let mut h = Hands::open()?;
            for line in &args[1..] {
                println!("{}", h.say(line)?);
            }
        }
        other => return Err(format!("I don't know «{other}»: pleamar-wm agent help")),
    }
    Ok(())
}
