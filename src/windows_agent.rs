//! Named scene operations and explicitly enabled foreground input.
use super::*;
#[path = "windows_agent_input.rs"]
mod input;

// Dialogs are addressable by an exact identity, but never enter tiling/recovery.
fn windows() -> Result<Vec<Window>> { super::catalog(true) }
fn target(id: &str) -> Result<(HWND, Window)> { super::target_kind(id, true) }

const HELP: &str = "pleamar-wm agent — native Windows scene commands

  scenes                         running pleamar scenes, their PIDs and command endpoints (JSON)
  windows                        ordinary native windows, with a scene name where known (JSON)
  monitors                       native monitor catalog (JSON)
  look PID|WINDOW_ID [FILE]       capture one visible ordinary window to a new PNG
                                 use window.id from windows when a process has several windows
  send PID|WINDOW_ID MONITOR      send a normal window to a connected monitor number/name
                                 preserves logical size and requests no activation
  tree PID [json]                visible elements, labels, states and logical geometry
  press PID NAME [right|middle] [COUNT]
                                 press a named scene element and report what changed
  wait PID CONDITION [TIMEOUT]    wait for a scene condition, for example saved == true 3s
  watch PID [SECONDS]            stream events and changes as they happen
  say PID ORDER...               another scene order: type query words, drag knob 0 -40,
                                 hold card, wheel list -3, key escape

PID comes from scenes or windows. PID.N also addresses that process's scene.
If one process has several endpoints, use scene:ENDPOINT to select one.
Panels such as Marea may appear only in scenes, not the ordinary window catalog.
This does not provide Linux's independent pointer, keyboard seat or cursor glide.
Look uses native WGC without activating or restoring the window. Hidden/minimized
windows and ambiguous PIDs are rejected. The output file must not exist.
Native application input is opt-in, in a separate terminal:
  serve --input foreground --monitor NAME [--process PID] [--seconds N]
                                 enable shared foreground input for 300 seconds (maximum 3600)
  input-status                   show the running broker's scope
  focus PID|WINDOW_ID             request focus for a visible window within that scope
  move PID X Y                   move the shared pointer within the last picture
  click PID X Y [BUTTON] [COUNT]  click in the last picture's physical pixel coordinates
  drag PID X1 Y1 X2 Y2            left-button drag in that picture
  scroll PID X Y DIRECTION [N]    up/down/left/right wheel, 1..30 steps
  type PID TEXT                  Unicode text; - reads stdin, maximum 4000 characters
  key PID NAME                   enter/tab/escape/backspace/arrows and navigation keys
  hotkey PID MODS+KEY             ctrl/alt/shift shortcuts, for example ctrl+a
  done                           discard every picture and input permit
  stop                           cancel the broker, including an in-flight capture/input

While the broker runs, look returns JSON and grants one input action within 30s.
Look again after every action. A moved, hidden, covered or unfocused target refuses
input. Dialogs are listed but never selected implicitly; select their exact window.id.
Use the exact display NAME, not all.
This shares your real pointer/keyboard; it is not Linux's independent input seat.
Open/background launch and remote control remain unavailable.
All commands use the current logon's scene namespace.";

#[derive(Clone, Debug, Serialize, PartialEq)]
struct Scene { pid: u32, scene: String, endpoint: String }

fn hello(endpoint: &str, answer: &str, peer: u32) -> Option<Scene> {
    if !answer.starts_with("pleamar ") || peer == 0 { return None; }
    let (_, rest) = answer.split_once(" · scene ")?;
    // Scene filenames may themselves contain the protocol's separator words.
    let (scene, identity) = rest.rsplit_once(" · pid ")?;
    let (reported, _) = identity.split_once(" · language ")?;
    if reported.trim().parse::<u32>().ok()? != peer || scene.is_empty() { return None; }
    Some(Scene { pid: peer, scene: scene.to_owned(), endpoint: endpoint.to_owned() })
}

fn scenes() -> Result<Vec<Scene>> {
    let mut found = Vec::new();
    let until = Instant::now() + Duration::from_secs(4);
    for endpoint in pleamar::commands::running_scenes() {
        if Instant::now() >= until { return Err("scene discovery timed out; close unresponsive command endpoints and retry".into()); }
        if let Ok((pid, answer)) = pleamar::commands::ask_with_pid(&endpoint, "hello", Duration::from_millis(250)) {
            if let Some(scene) = hello(&endpoint, &answer, pid) { found.push(scene); }
        }
    }
    Ok(found)
}

fn select<'a>(scenes: &'a [Scene], selector: &str) -> Result<&'a Scene> {
    if let Some(endpoint) = selector.strip_prefix("scene:") {
        return scenes.iter().find(|s| s.endpoint == endpoint).ok_or_else(|| "that scene endpoint did not answer hello".into());
    }
    let mut parts = selector.split('.');
    let pid = parts.next().and_then(|n| n.parse::<u32>().ok()).filter(|n| *n > 0).ok_or("use a PID from agent scenes")?;
    if let Some(index) = parts.next() {
        if index.parse::<u32>().is_err() || parts.next().is_some() { return Err("use PID or PID.N from agent windows".into()); }
    }
    let mut matches = scenes.iter().filter(|s| s.pid == pid);
    let first = matches.next().ok_or("this process has no responding pleamar scene; native application input requires the foreground broker described by agent help")?;
    if matches.next().is_some() { return Err("this process has several scenes; use scene:ENDPOINT from agent scenes".into()); }
    Ok(first)
}

fn select_window<'a>(catalog: &'a [Window], selector: &str) -> Result<&'a Window> {
    if selector.contains(':') {
        return catalog.iter().find(|w| w.id == selector).ok_or_else(|| "window identity is no longer in the visible native catalog".into());
    }
    let pid = selector.parse::<u32>().ok().filter(|n| *n > 0)
        .ok_or("use a PID or exact window.id from agent windows; PID.N is not a native window identity")?;
    let mut matches = catalog.iter().filter(|w| w.process == pid);
    let first = matches.next().ok_or("process has no visible ordinary window")?;
    if matches.next().is_some() { return Err("process has several windows; use the exact window.id from agent windows".into()); }
    Ok(first)
}

fn send_monitor<'a>(screens:&'a [Monitor],selector:&str) -> Result<&'a Monitor> {
    screens.iter().enumerate().find(|(i,m)|m.name==selector || i.to_string()==selector)
        .map(|(_,m)|m).ok_or_else(||"destination is not connected; use agent monitors".into())
}

fn send_window(selector:&str,monitor:&str) -> Result<Value> {
    let catalog=windows()?;
    let selected=select_window(&catalog,selector)?;
    let screens=monitors()?;
    let to=send_monitor(&screens,monitor)?;
    let (_,window)=target(&selected.id)?;
    normal(&window)?;
    let from=screens.iter().find(|m|m.name==window.monitor).ok_or("source monitor disconnected")?;
    if from.name!=to.name {
        let bounds=transfer::geometry(&window.bounds,from,to,16_777_216)?;
        place(&window.id,&bounds)?;
    }
    let (_,current)=target(&window.id)?;
    if current.monitor!=to.name { return Err("window did not reach the requested monitor".into()); }
    Ok(json!({"id":current.id,"monitor":current.monitor,"bounds":current.bounds}))
}

fn png(picture: capture::Picture) -> Result<Vec<u8>> {
    use image::ImageEncoder;
    let (width,height) = picture.size;
    if width == 0 || height == 0 || width > 8192 || height > 8192 { return Err("invalid native capture dimensions".into()); }
    let length = u64::from(width)*u64::from(height)*4;
    if length > 16_777_216*4 || length != picture.pixels.len() as u64 || picture.shared.is_some() {
        return Err("invalid native capture pixel buffer".into());
    }
    let mut rgba = picture.pixels;
    for p in rgba.chunks_exact_mut(4) { p.swap(0,2); }
    let mut encoded = Vec::new();
    image::codecs::png::PngEncoder::new(&mut encoded).write_image(&rgba,width,height,image::ExtendedColorType::Rgba8)?;
    Ok(encoded)
}

fn write_picture(path: &Path, encoded: &[u8]) -> Result<()> {
    use std::io::Write;
    // Never replace a previous capture or an unrelated user file.
    let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(path)?;
    file.write_all(encoded)?;
    file.sync_all()?;
    Ok(())
}

fn look(selector: &str, output: Option<&str>) -> Result<()> {
    if input::look_if_running(selector, output)? { return Ok(()); }
    let catalog = windows()?;
    let window = select_window(&catalog,selector)?;
    if window.minimized { return Err("window is minimized; look does not restore or focus it".into()); }
    let (hwnd,_) = target(&window.id)?;
    let mut affinity = 0;
    if unsafe { GetWindowDisplayAffinity(hwnd,&mut affinity) }.is_ok() && affinity != WDA_NONE.0 {
        return Err("window excludes itself from capture".into());
    }
    let wake = wait::Wake::new()?;
    let waiter = wait::Waiter::new(wake.clone())?;
    let device = capture::Device::new(Some(wake))?;
    let mut source = capture::Capture::new(device,hwnd,16_777_216)?;
    let deadline = Instant::now()+Duration::from_secs(6);
    let picture = loop {
        if let Some(picture) = source.next(16_777_216)? { break picture; }
        let now = Instant::now();
        if now >= deadline { return Err("native window capture timed out without a frame".into()); }
        let delay = if source.pending() { Duration::from_millis(8).min(deadline-now) } else { deadline-now };
        waiter.wait(delay)?;
        let mut message = MSG::default();
        while unsafe { PeekMessageW(&mut message,None,0,0,PM_REMOVE) }.as_bool() {
            unsafe { let _=TranslateMessage(&message);DispatchMessageW(&message); }
        }
    };
    let (_,current) = target(&window.id)?;
    if source.closed() || current.minimized { return Err("window closed or became minimized during capture".into()); }
    let size = picture.size;
    drop(source);
    let encoded = png(picture)?;
    let path = match output {
        Some(path) => std::path::PathBuf::from(path),
        None => {
            let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
            std::env::temp_dir().join(format!("pleamar-agent-look-{}-{stamp}.png",std::process::id()))
        },
    };
    write_picture(&path,&encoded)?;
    println!("{} {}x{}",path.display(),size.0,size.1);
    Ok(())
}

pub(super) fn execute(args: &[&str]) -> Result<Option<Value>> {
    match args {
        [] | ["help" | "--help" | "-h"] => { println!("{HELP}"); Ok(None) },
        ["serve" | "stop" | "done" | "input-status" | "focus" | "move" | "click" | "drag" | "scroll" | "type" | "key" | "hotkey", ..] => input::execute(args).map(Some),
        ["scenes"] => Ok(Some(serde_json::to_value(scenes()?)?)),
        ["monitors"] => Ok(Some(serde_json::to_value(monitors()?)?)),
        ["look", selector] => { look(selector,None)?; Ok(None) },
        ["look", selector, file] => { look(selector,Some(file))?; Ok(None) },
        ["send", selector, monitor] => send_window(selector,monitor).map(Some),
        ["windows"] => {
            let scenes = scenes()?;
            let list = windows()?.into_iter().map(|w| {
                let matching: Vec<_> = scenes.iter().filter(|s| s.pid == w.process).collect();
                json!({"window":w,"scenes":matching})
            }).collect::<Vec<_>>();
            Ok(Some(json!(list)))
        },
        [what @ ("tree" | "press" | "wait" | "watch" | "say"), selector, rest @ ..] => {
            let line = match (*what, rest) {
                ("tree", []) => "describe".to_owned(),
                ("tree", ["json"]) => "describe json".to_owned(),
                ("tree", _) => return Err("tree PID [json]".into()),
                ("watch", [] | [_]) => format!("watch {}", rest.join(" ")),
                ("press" | "wait" | "say", []) => return Err(format!("{what} needs a scene command or element").into()),
                ("say", _) => rest.join(" "),
                ("press" | "wait", _) => format!("{what} {}", rest.join(" ")),
                _ => return Err("watch PID [SECONDS]".into()),
            };
            let scenes = scenes()?;
            let scene = select(&scenes, selector)?;
            pleamar::commands::send_to_process(&scene.endpoint, scene.pid, line.trim()).map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
            Ok(None)
        },
        _ => Err("unsupported Windows agent operation; use agent help for native scene commands and remaining limitations".into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hello_keeps_unicode_names_and_rejects_other_protocols() {
        let answer = "pleamar 0.2.25 · scene Mi escena ñ · pid 123 · language 0.2";
        assert_eq!(hello("Mi escena ñ-123", answer, 123), Some(Scene {pid:123,scene:"Mi escena ñ".into(),endpoint:"Mi escena ñ-123".into()}));
        let embedded = "pleamar 0.2.25 · scene ñ · pid 999 · scene 海 · pid 123 · language 0.2";
        assert_eq!(hello("endpoint", embedded, 123).unwrap().scene, "ñ · pid 999 · scene 海");
        assert!(hello("endpoint", embedded, 999).is_none());
        for answer in ["? unknown hello", "pleamar x · scene x · pid 0", "other x · scene x · pid 123", "pleamar x · pid 123"] {
            assert_eq!(hello("x", answer, 123), None);
        }
    }
    #[test]
    fn process_selection_refuses_ambiguity_and_malformed_numbers() {
        let mut scenes = vec![Scene {pid:123,scene:"Marea".into(),endpoint:"Marea".into()}];
        assert_eq!(select(&scenes,"123.2").unwrap().endpoint,"Marea");
        for selector in ["0","123.","123.2.3","-123","456"] { assert!(select(&scenes,selector).is_err()); }
        scenes.push(Scene {pid:123,scene:"Second".into(),endpoint:"Second".into()});
        assert!(select(&scenes,"123").is_err());
        assert_eq!(select(&scenes,"scene:Marea").unwrap().scene,"Marea");
    }
    #[test]
    fn native_capture_selection_requires_an_unambiguous_current_identity() {
        let first = Window { id:"123:4:500:abc".into(),title:"First ñ".into(),app:"owned.exe".into(),class:"test".into(),
            process:123,monitor:"DISPLAY2".into(),bounds:Bounds{x:0,y:0,width:100,height:100},minimized:false,maximized:false,resizable:true };
        let mut catalog = vec![first.clone()];
        assert_eq!(select_window(&catalog,"123").unwrap().id,first.id);
        for selector in ["0","-1","123.1","123:4:500:old","456","scene:panel"] {
            assert!(select_window(&catalog,selector).is_err(),"{selector}");
        }
        let mut second=first.clone();second.id="123:5:600:abc".into();catalog.push(second);
        assert!(select_window(&catalog,"123").is_err());
        assert_eq!(select_window(&catalog,&first.id).unwrap().title,"First ñ");
        catalog.remove(0);
        assert!(select_window(&catalog,&first.id).is_err());
    }
    #[test]
    fn native_send_uses_the_current_monitor_catalog_and_refuses_missing_outputs() {
        let screen=|name:&str,x|Monitor{name:name.into(),bounds:Bounds{x,y:0,width:1920,height:1080},
            work:Bounds{x,y:0,width:1920,height:1040},scale:1.0,primary:false,refresh_hz:60};
        let screens=[screen(r"\\.\DISPLAY2",-1920),screen(r"\\.\DISPLAY1",0)];
        assert_eq!(send_monitor(&screens,"0").unwrap().name,r"\\.\DISPLAY2");
        assert_eq!(send_monitor(&screens,r"\\.\DISPLAY1").unwrap().name,r"\\.\DISPLAY1");
        for selector in ["", "-1", "2", "999", "1.0", r"\\.\DISPLAY9"] { assert!(send_monitor(&screens,selector).is_err()); }
        assert!(send_monitor(&[],"0").is_err());
    }
    #[test]
    fn png_keeps_native_color_channels_and_never_replaces_a_file() {
        let encoded=png(capture::Picture {size:(2,1),pixels:vec![0x60,0xc0,0x20,255,0x80,0x30,0xd0,255],shared:None}).unwrap();
        let decoded=image::load_from_memory(&encoded).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(),(2,1));
        assert_eq!(decoded.into_raw(),[0x20,0xc0,0x60,255,0xd0,0x30,0x80,255]);
        for (size,pixels) in [((0,1),vec![]),((2,1),vec![0;4]),((u32::MAX,u32::MAX),vec![])] {
            assert!(png(capture::Picture{size,pixels,shared:None}).is_err());
        }
        let stamp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let folder=std::env::temp_dir().join(format!("wm-look-{}-{stamp}",std::process::id()));
        std::fs::create_dir(&folder).unwrap();
        let file=folder.join("Picture ñ 海.png");
        write_picture(&file,&encoded).unwrap();
        assert!(write_picture(&file,b"replacement").is_err());
        assert_eq!(std::fs::read(&file).unwrap(),encoded);
        assert!(write_picture(&folder.join("missing").join("picture.png"),&encoded).is_err());
        std::fs::remove_file(file).unwrap();std::fs::remove_dir(folder).unwrap();
    }
}
