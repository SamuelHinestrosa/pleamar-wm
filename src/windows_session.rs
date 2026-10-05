//! Event-driven per-monitor layouts. The recovery journal is flushed before
//! changing a window; ending a session restores free positions, not focus.
use super::*;
use std::{cell::Cell, collections::{BTreeMap, BTreeSet}, io::Write, os::windows::{fs::OpenOptionsExt, ffi::OsStrExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}},
    path::PathBuf, sync::atomic::Ordering};
use windows::Win32::{Storage::FileSystem::*, UI::Accessibility::*};

thread_local! {
    static DIRTY: Cell<bool> = const { Cell::new(false) };
    static DRAGGING: Cell<bool> = const { Cell::new(false) };
}
unsafe extern "system" fn changed(_: HWINEVENTHOOK, event: u32, _: HWND, object: i32, child: i32, _: u32, _: u32) {
    if event == EVENT_SYSTEM_MOVESIZESTART { DRAGGING.set(true); }
    if event == EVENT_SYSTEM_MOVESIZEEND { DRAGGING.set(false); }
    if event < EVENT_OBJECT_CREATE || (object == 0 && child == 0) { DIRTY.set(true); }
}
struct Hooks(Vec<HWINEVENTHOOK>);
impl Hooks {
    fn new(process: u32) -> Result<Self> {
        let mut hooks = Self(Vec::new());
        for (first, last) in [(EVENT_OBJECT_CREATE, EVENT_OBJECT_LOCATIONCHANGE),
            (EVENT_SYSTEM_MOVESIZESTART, EVENT_SYSTEM_MOVESIZEEND), (EVENT_SYSTEM_MINIMIZESTART, EVENT_SYSTEM_MINIMIZEEND)] {
            let hook = unsafe { SetWinEventHook(first, last, None, Some(changed), process, 0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS) };
            if hook.is_invalid() { return Err("could not subscribe to native window events".into()); }
            hooks.0.push(hook);
        }
        Ok(hooks)
    }
}
impl Drop for Hooks { fn drop(&mut self) { for hook in self.0.drain(..) { let _ = unsafe { UnhookWinEvent(hook) }; } } }

#[derive(Clone, Serialize, Deserialize)]
struct Original { id: String, monitor: String, bounds: Bounds, normal: [i32; 4] }
#[derive(Serialize, Deserialize)]
struct Recovery { version: u32, windows: Vec<Original> }

struct Journal { path: PathBuf, _lock: std::fs::File }
impl Journal {
    fn open(path: PathBuf) -> Result<(Self, BTreeMap<String, Original>)> {
        let parent = path.parent().ok_or("WM state file needs a parent directory")?;
        std::fs::create_dir_all(parent)?;
        let lock = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false)
            .share_mode(0).open(path.with_extension("lock"))?;
        let mut windows = BTreeMap::new();
        if path.exists() {
            let file = std::fs::File::open(&path)?;
            if file.metadata()?.len() > 65536 { return Err("WM recovery journal is too large".into()); }
            let saved: Recovery = serde_json::from_reader(file)?;
            if saved.version != 1 || saved.windows.len() > 64 { return Err("unknown WM recovery journal".into()); }
            for item in saved.windows {
                if item.bounds.width <= 0 || item.bounds.height <= 0 || windows.insert(item.id.clone(), item).is_some() {
                    return Err("invalid WM recovery journal".into());
                }
            }
        }
        Ok((Self { path, _lock:lock }, windows))
    }
    fn save(&self, windows: &BTreeMap<String, Original>) -> Result<()> {
        if windows.len() > 64 { return Err("WM recovery supports at most 64 managed windows".into()); }
        let temporary = self.path.with_extension(format!("{}.pending", std::process::id()));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).open(&temporary)?;
        let result = (|| -> Result<()> {
            let bytes = serde_json::to_vec(&Recovery { version:1, windows:windows.values().cloned().collect() })?;
            if bytes.len() > 65536 { return Err("WM recovery journal exceeds its limit".into()); }
            file.write_all(&bytes)?;
            file.sync_all()?;
            drop(file);
            let from: Vec<u16> = temporary.as_os_str().encode_wide().chain([0]).collect();
            let to: Vec<u16> = self.path.as_os_str().encode_wide().chain([0]).collect();
            unsafe { MoveFileExW(PCWSTR(from.as_ptr()), PCWSTR(to.as_ptr()), MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH) }?;
            Ok(())
        })();
        if result.is_err() { let _ = std::fs::remove_file(&temporary); }
        result
    }
}

struct Mode { tiled: bool, layout: Layout, order: Vec<String>, error: Option<String> }
impl Default for Mode { fn default() -> Self { Self { tiled:false, layout:Layout::Left, order:Vec::new(), error:None } } }

fn placement(hwnd: HWND) -> Result<WINDOWPLACEMENT> {
    let mut p = WINDOWPLACEMENT { length:size_of::<WINDOWPLACEMENT>() as u32, ..Default::default() };
    unsafe { GetWindowPlacement(hwnd, &mut p) }?;
    Ok(p)
}
fn rect_array(rect: RECT) -> [i32; 4] { [rect.left, rect.top, rect.right, rect.bottom] }
fn original(id: &str) -> Result<Original> {
    let (hwnd, w) = target(id)?;
    Ok(Original { id:id.into(), monitor:w.monitor, bounds:w.bounds, normal:rect_array(placement(hwnd)?.rcNormalPosition) })
}

fn restore(old: &Original, screens: &[Monitor]) -> Result<bool> {
    let handle = old.id.split(':').nth(2).and_then(|s| usize::from_str_radix(s, 16).ok())
        .ok_or("invalid recovery window id")?;
    let hwnd = HWND(handle as _);
    if !Identity::read(hwnd).is_some_and(|i| i.token() == old.id) { return Ok(true); }
    let Some(screen) = screens.iter().find(|m| m.name == old.monitor) else { return Ok(false); };
    let Ok((hwnd, current)) = target(&old.id) else { return Ok(false); };
    // A move to another still-connected monitor is the user's new free position.
    if current.monitor != old.monitor { return Ok(true); }
    if !screen.bounds.contains(&old.bounds) { return Ok(false); }
    if !current.minimized && !current.maximized { place(&old.id, &old.bounds)?; return Ok(true); }
    let mut p = placement(hwnd)?;
    p.rcNormalPosition = RECT { left:old.normal[0], top:old.normal[1], right:old.normal[2], bottom:old.normal[3] };
    p.flags |= WPF_ASYNCWINDOWPLACEMENT;
    p.showCmd = if current.minimized { SW_SHOWMINNOACTIVE.0 as u32 } else { SW_SHOWNA.0 as u32 };
    unsafe { SetWindowPlacement(hwnd, &p) }?;
    let until = Instant::now() + Duration::from_secs(1);
    loop {
        pump();
        let (_, now) = target(&old.id)?;
        if rect_array(placement(hwnd)?.rcNormalPosition) == old.normal
            && now.minimized == current.minimized && now.maximized == current.maximized { return Ok(true); }
        if Instant::now() >= until { return Err("window did not confirm its restored free position".into()); }
        std::thread::sleep(Duration::from_millis(10));
    }
}

struct Manager {
    modes: BTreeMap<String, Mode>, originals:BTreeMap<String, Original>, journal:Journal,
    process:Option<u32>, owner:Option<u32>, creation:Option<u64>, all:bool, scans:u64, changes:u64,
}
impl Manager {
    fn owns(&self, window: &Window) -> bool {
        self.process.is_none_or(|pid| window.process == pid)
            && self.creation.is_none_or(|created| window.id.ends_with(&format!(":{created:x}")))
    }
    fn open(options: &Options) -> Result<Self> {
        let screens = monitors()?;
        let modes: BTreeMap<_, _> = screens.iter().filter(|m| options.all || options.monitors.contains(&m.name))
            .map(|m| (m.name.clone(), Mode::default())).collect();
        if !options.all && modes.len() != options.monitors.len() { return Err("a requested monitor is not connected".into()); }
        let (journal, originals) = Journal::open(options.state.clone())?;
        let mut manager = Self { modes, originals, journal, process:options.process, owner:options.owner, creation:None, all:options.all, scans:0, changes:0 };
        if let Some(pid) = options.process {
            // Store a creation stamp as well: a recycled PID must not widen a fixture's scope.
            manager.creation = windows()?.iter().find(|w| w.process == pid)
                .and_then(|w| w.id.rsplit(':').next()).and_then(|s| u64::from_str_radix(s, 16).ok());
            if manager.creation.is_none() { return Err("scoped process has no eligible window".into()); }
        }
        manager.release(None)?;
        Ok(manager)
    }
    fn release(&mut self, screen:Option<&str>) -> Result<()> {
        let screens = monitors()?;
        let before = self.originals.len();
        let mut errors = Vec::new();
        let ids: Vec<_> = self.originals.values().filter(|w| screen.is_none_or(|s| w.monitor == s)).map(|w| w.id.clone()).collect();
        for id in ids {
            let old = &self.originals[&id];
            if !self.modes.contains_key(&old.monitor) { continue; }
            if let Ok((_, w)) = target(&id) { if !self.owns(&w) { continue; } }
            match restore(old, &screens) {
                Ok(true) => { self.originals.remove(&id); }
                Ok(false) => {}
                Err(e) => errors.push(e.to_string()),
            }
        }
        if before != self.originals.len() || !self.journal.path.exists() { self.journal.save(&self.originals)?; }
        if !errors.is_empty() { return Err(errors.join("; ").into()); }
        Ok(())
    }
    fn status(&self) -> Value {
        json!({"running":true,"automatic_layouts":true,"rain":false,"snow":false,"ride":false,"dock":false,
            "pools":false,"process":self.process,"owner":self.owner,"saved_windows":self.originals.len(),
            "pending_recovery":self.originals.values().filter(|w|self.modes.get(&w.monitor).is_none_or(|m|!m.tiled)).count(),
            "catalog_scans":self.scans,"geometry_changes":self.changes,
            "monitors":self.modes.iter().map(|(name,m)| json!({"name":name,"tiled":m.tiled,
                "layout":format!("{:?}",m.layout).to_lowercase(),"windows":m.order.len(),"error":m.error})).collect::<Vec<_>>()})
    }
    fn set_mode(&mut self, name:&str, tiled:bool, layout:Option<Layout>) -> Result<Value> {
        let mode = self.modes.get_mut(name).ok_or("monitor is outside this WM session")?;
        mode.tiled = tiled;
        mode.error = None;
        if let Some(layout) = layout { mode.layout = layout; }
        if tiled { self.reconcile()?; }
        else { mode.order.clear(); self.release(Some(name))?; }
        if let Some(e) = &self.modes[name].error { return Err(e.clone().into()); }
        Ok(self.status())
    }
    fn reconcile(&mut self) -> Result<()> {
        let screens = monitors()?;
        if self.all { for screen in &screens { self.modes.entry(screen.name.clone()).or_default(); } }
        if self.originals.is_empty() && self.modes.values().all(|m| !m.tiled) { return Ok(()); }
        self.scans += 1;
        let live: Vec<_> = windows()?.into_iter().filter(|w| self.owns(w)).collect();
        // Forget only destroyed identities or deliberate moves; hidden/minimized
        // windows retain their recovery position until they return or we exit.
        let previous = self.originals.len();
        self.originals.retain(|id, old| {
            let handle = id.split(':').nth(2).and_then(|s| usize::from_str_radix(s,16).ok()).unwrap_or(0);
            Identity::read(HWND(handle as _)).is_some_and(|i| i.token() == *id)
                && !live.iter().any(|w| w.id == *id && w.monitor != old.monitor && screens.iter().any(|m| m.name == old.monitor))
        });
        if self.originals.len() != previous { self.journal.save(&self.originals)?; }
        let names:Vec<_> = self.modes.keys().cloned().collect();
        for name in names {
            if !self.modes[&name].tiled {
                if self.originals.values().any(|w| w.monitor == name) { self.release(Some(&name))?; }
                continue;
            }
            let Some(screen) = screens.iter().find(|m| m.name == name) else { continue; };
            let eligible:Vec<_> = live.iter().filter(|w| w.monitor == name && !w.minimized && !w.maximized && w.resizable).collect();
            let mode = self.modes.get_mut(&name).unwrap();
            mode.order.retain(|id| eligible.iter().any(|w| w.id == *id));
            let mut added:Vec<_> = eligible.iter().filter(|w| !mode.order.contains(&w.id)).map(|w|w.id.clone()).collect();
            added.sort();
            mode.order.extend(added);
            if mode.order.is_empty() { continue; }
            let order = mode.order.clone();
            let layout = mode.layout;
            let changed = (|| -> Result<()> {
                let boxes = layout::arrange((&screen.work).into(), order.len(), layout, (8.0*screen.scale).round() as i32)?;
                let added = order.iter().filter(|id| !self.originals.contains_key(*id)).count();
                if self.originals.len() + added > 64 { return Err("WM recovery supports at most 64 managed windows across monitors".into()); }
                let mut saved = false;
                for id in &order {
                    if !self.originals.contains_key(id) {
                        let value = original(id)?;
                        if !screen.bounds.contains(&value.bounds) { return Err("bring new windows wholly onto their monitor before tiling".into()); }
                        self.originals.insert(id.clone(), value); saved = true;
                    }
                }
                if saved { self.journal.save(&self.originals)?; }
                for (id, rect) in order.iter().zip(boxes) {
                    let bounds:Bounds = rect.into();
                    if target(id)?.1.bounds != bounds { place(id, &bounds)?; self.changes += 1; }
                }
                Ok(())
            })();
            if let Err(error) = changed {
                self.modes.get_mut(&name).unwrap().tiled = false;
                let recovery = self.release(Some(&name)).err().map(|e|format!("; restore: {e}")).unwrap_or_default();
                self.modes.get_mut(&name).unwrap().error = Some(format!("{error}{recovery}"));
            }
        }
        Ok(())
    }
    fn command(&mut self, line:&str) -> Result<(Value,bool)> {
        let words:Vec<_> = line.split_whitespace().collect();
        let value = match words.as_slice() {
            ["status"] | ["capabilities"] => self.status(),
            ["quit"] => {
                for mode in self.modes.values_mut() { mode.tiled=false; mode.order.clear(); }
                self.release(None)?;
                return Ok((json!({"stopped":true,"pending_recovery":self.originals.len()}),true));
            }
            ["toggle", name] => {
                let tiled = !self.modes.get(*name).ok_or("unknown WM monitor")?.tiled;
                self.set_mode(name,tiled,None)?
            }
            ["layout", name, kind] => self.set_mode(name,true,Some(kind.parse()?))?,
            ["free", name] => self.set_mode(name,false,None)?,
            ["emit", "toggle_free"] => {
                let mut point = POINT::default();
                unsafe { GetCursorPos(&mut point) }?;
                let m = monitor(unsafe { MonitorFromPoint(point, MONITOR_DEFAULTTONULL) }).ok_or("pointer has no active monitor")?;
                let tiled = !self.modes.get(&m.name).ok_or("pointer is outside this WM session")?.tiled;
                self.set_mode(&m.name,tiled,None)?
            }
            _ => return Err(format!("unsupported Windows WM command: {line}").into()),
        };
        Ok((value,false))
    }
}

// Hold the process object, not its PID: a recycled PID cannot keep a session alive.
struct Owner(OwnedHandle);
impl Owner {
    fn open(pid:u32) -> Result<Self> {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) }?;
        let owner = Self(unsafe { OwnedHandle::from_raw_handle(handle.0) });
        if owner.exited()? { return Err("WM owner has already exited".into()); }
        Ok(owner)
    }
    fn handle(&self) -> HANDLE { HANDLE(self.0.as_raw_handle()) }
    fn exited(&self) -> Result<bool> {
        match unsafe { WaitForSingleObject(self.handle(), 0) } {
            WAIT_OBJECT_0 => Ok(true),
            WAIT_TIMEOUT => Ok(false),
            _ => Err(std::io::Error::last_os_error().into()),
        }
    }
}

struct Options { monitors:BTreeSet<String>, all:bool, process:Option<u32>, owner:Option<u32>, state:PathBuf, namespace:String, seconds:Option<u64> }
impl Options {
    fn parse(args:&[String]) -> Result<Self> {
        let mut options = Self { monitors:BTreeSet::new(), all:false, process:None, owner:None,
            state:pleamar::config_dir().ok_or("Windows configuration directory unavailable")?.join("wm/windows-session.json"),
            namespace:std::env::var("PLEAMAR_WM_NAMESPACE").unwrap_or_default(), seconds:None };
        let mut explicit_state = false;
        let mut it = args.iter();
        while let Some(key) = it.next() {
            let value = it.next().ok_or("session options need values")?;
            match key.as_str() {
                "--monitor" => if value == "all" { options.all = true; } else { options.monitors.insert(value.clone()); },
                "--process" => options.process = Some(value.parse()?),
                "--owner" => options.owner = Some(value.parse()?),
                "--state" => { options.state = std::path::absolute(value)?; explicit_state = true; },
                "--namespace" => options.namespace = value.clone(),
                "--seconds" => { let seconds = value.parse()?; if !(1..=86400).contains(&seconds) { return Err("invalid session duration".into()); } options.seconds=Some(seconds); },
                _ => return Err(format!("unknown session option: {key}").into()),
            }
        }
        if !options.all && options.monitors.is_empty() { return Err("session requires --monitor NAME or --monitor all".into()); }
        if options.process == Some(0) { return Err("process scope must be a nonzero PID".into()); }
        if options.owner == Some(0) { return Err("owner must be a nonzero PID".into()); }
        if !explicit_state && !options.namespace.is_empty() {
            // Validate before allowing the namespace to become part of a filename.
            ipc::Endpoint::new(&options.namespace)?;
            options.state.set_file_name(format!("windows-session-{}.json", options.namespace));
        }
        Ok(options)
    }
}

pub(super) fn run(args:&[String]) -> Result<Value> {
    let options = Options::parse(args)?;
    let owner = options.owner.map(Owner::open).transpose()?;
    let server = ipc::Server::start(ipc::Endpoint::new(&options.namespace)?)?;
    let mut manager = Manager::open(&options)?;
    let _hooks = Hooks::new(options.process.unwrap_or(0))?;
    let started = Instant::now();
    let mut dirty_since = None;
    let mut last_topology = Instant::now();
    let mut topology = serde_json::to_string(&monitors()?)?;
    let mut handles = vec![server.wake.handle()];
    if let Some(owner) = &owner { handles.push(owner.handle()); }
    let result = (|| -> Result<()> {
        loop {
            pump();
            if owner.as_ref().map(Owner::exited).transpose()?.unwrap_or(false) { break; }
            if !server.running() { return Err("WM command listener stopped unexpectedly".into()); }
            server.wake.reset();
            let mut quit = false;
            while let Ok(request) = server.requests.try_recv() {
                if request.expired.load(Ordering::Acquire) { continue; }
                let response = manager.command(&request.command).map(|(value,stop)| { quit |= stop; value });
                request.finish(response);
                if quit { break; }
            }
            if quit || options.seconds.is_some_and(|s| started.elapsed() >= Duration::from_secs(s)) { break; }
            if DIRTY.replace(false) { dirty_since.get_or_insert_with(Instant::now); }
            if last_topology.elapsed() >= Duration::from_secs(2) {
                let current = serde_json::to_string(&monitors()?)?;
                if current != topology { dirty_since.get_or_insert_with(Instant::now); topology=current; }
                last_topology=Instant::now();
            }
            if !DRAGGING.get() && dirty_since.is_some_and(|t| t.elapsed() >= Duration::from_millis(60)) {
                dirty_since=None;
                manager.reconcile()?;
            }
            let wait = if dirty_since.is_some() { 60 } else { 500 };
            let result = unsafe { MsgWaitForMultipleObjectsEx(Some(&handles),wait,QS_ALLINPUT,MWMO_INPUTAVAILABLE) };
            if result == WAIT_FAILED { return Err(std::io::Error::last_os_error().into()); }
        }
        Ok(())
    })();
    let restore = manager.release(None);
    result?;
    restore?;
    Ok(json!({"stopped":true,"pending_recovery":manager.originals.len()}))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_names_separate_default_journals_and_reject_path_injection() {
        let parse = |args:&[&str]| Options::parse(&args.iter().map(|v|v.to_string()).collect::<Vec<_>>());
        let one = parse(&["--monitor","all","--namespace","one"]).unwrap();
        let two = parse(&["--monitor","all","--namespace","two"]).unwrap();
        assert_ne!(one.state,two.state);
        assert_eq!(one.state.file_name().unwrap(),"windows-session-one.json");
        assert!(parse(&["--monitor","all","--namespace","../escape"]).is_err());
        assert!(parse(&["--monitor","all","--owner","0"]).is_err());
        let explicit = parse(&["--monitor","all","--namespace","one","--state","explicit.json"]).unwrap();
        assert_eq!(explicit.state,std::path::absolute("explicit.json").unwrap());
    }
}
