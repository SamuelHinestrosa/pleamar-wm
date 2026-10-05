//! Live native window pictures, with optional native window actions. Input
//! inside an application still goes through its original Windows window.
use super::*;
use pleamar::scene::{NestEvent, PieceContent, ToNest, ToRender, WindowPiece};
use std::{cell::Cell, sync::{OnceLock, mpsc::{self, Sender, TryRecvError}}};
use windows::Win32::UI::Accessibility::*;

#[path = "windows_configure.rs"]
mod configure;

#[derive(Clone)]
struct Scope { monitor:String, process:Option<u32>, actions:bool }
impl Scope {
    fn allows(&self, window:&Window, created:Option<u64>) -> bool {
        window.monitor==self.monitor && self.process.is_none_or(|pid|window.process==pid)
            && created.is_none_or(|stamp|window.id.ends_with(&format!(":{stamp:x}")))
    }
}
static SCOPE:OnceLock<Scope> = OnceLock::new();
thread_local! { static CATALOG_DIRTY:Cell<bool> = const { Cell::new(true) }; }
unsafe extern "system" fn changed(_:HWINEVENTHOOK,_:u32,_:HWND,object:i32,child:i32,_:u32,_:u32) {
    if object == 0 && child == 0 { CATALOG_DIRTY.set(true); }
}
struct Hooks(Vec<HWINEVENTHOOK>);
impl Hooks {
    fn add(&mut self, first:u32,last:u32,pid:u32) -> Result<()> {
        let hook=unsafe { SetWinEventHook(first,last,None,Some(changed),pid,0,WINEVENT_OUTOFCONTEXT|WINEVENT_SKIPOWNPROCESS) };
        if hook.is_invalid() { return Err("native window event subscription failed".into()); }
        self.0.push(hook);Ok(())
    }
}
impl Drop for Hooks { fn drop(&mut self) { for hook in self.0.drain(..) { let _=unsafe { UnhookWinEvent(hook) }; } } }

pub(super) fn prepare(args:&[String]) -> Result<Vec<String>> {
    let mut monitor=std::env::var("PLEAMAR_WM_PREVIEW_MONITOR").ok();
    let mut process=std::env::var("PLEAMAR_WM_PREVIEW_PROCESS").ok();
    let mut actions=false;
    let mut forwarded=Vec::new(); let mut it=args.iter();
    while let Some(argument)=it.next() {
        match argument.as_str() {
            "--preview-monitor" => monitor=Some(it.next().ok_or("preview monitor missing")?.clone()),
            "--preview-process" => process=Some(it.next().ok_or("preview process missing")?.clone()),
            "--window-actions" => actions=true,
            _ => forwarded.push(argument.clone()),
        }
    }
    let Some(monitor)=monitor else {
        if process.is_some() || actions { return Err("window previews and actions need an explicit preview monitor".into()); }
        return Ok(forwarded);
    };
    let monitor=select_monitor(&monitor)?.name;
    let process=process.map(|v|v.parse::<u32>()).transpose()?;
    if process==Some(0) || process==Some(std::process::id()) { return Err("invalid preview process".into()); }
    SCOPE.set(Scope { monitor,process,actions }).map_err(|_|"window preview is already configured")?;
    pleamar::provide_windows(start);
    Ok(forwarded)
}

fn start(max:usize,to_render:Sender<ToRender>) -> Option<pleamar::NestSender> {
    let scope=SCOPE.get()?.clone();
    let (commands,receive)=mpsc::channel();
    let (ready,started)=mpsc::sync_channel(1);
    let wake=wait::Wake::new().ok()?;
    let command_wake=wake.clone();
    std::thread::Builder::new().name("native-window-pictures".into()).spawn(move || {
        match Preview::new(max,scope,to_render.clone(),wake) {
            Ok(mut preview) => {
                let _=ready.send(Ok(()));
                if let Err(error)=preview.run(receive) { eprintln!("windows preview: {error}"); }
            },
            Err(error) => { let _=ready.send(Err(error.to_string())); },
        }
    }).ok()?;
    match started.recv_timeout(Duration::from_secs(10)) {
        Ok(Ok(())) => Some(Box::new(move |message| { let _=commands.send(message); command_wake.signal(); })),
        result => { eprintln!("windows preview could not start: {result:?}"); None },
    }
}

struct Slot { window:Window, capture:Option<capture::Capture>, received:bool, pixels:u64, born:Instant, next_frame:Instant,
    configure:configure::Configure }
impl Slot {
    // A minimized window still has its last texture in the renderer. It must
    // count towards the same bound even after its capture resources are freed.
    fn pixels(&self) -> u64 {
        self.pixels.max(self.configure.pixels())
            .max(self.capture.as_ref().and_then(|c|c.size().ok()).map_or(0,|(w,h)|w as u64*h as u64))
    }
}
struct Preview {
    scope:Scope, created:Option<u64>, device:std::rc::Rc<capture::Device>,
    slots:Vec<Option<Slot>>, send:Sender<ToRender>, _hooks:Hooks,
    consumed:Option<mpsc::Receiver<String>>, scale:f64, waiter:wait::Waiter, warned:HashSet<&'static str>,
    focused:Option<usize>,
}
impl Preview {
    fn new(max:usize,scope:Scope,send:Sender<ToRender>,wake:std::sync::Arc<wait::Wake>) -> Result<Self> {
        if !(1..=64).contains(&max) { return Err("invalid native window slot count".into()); }
        let created=match scope.process {
            Some(pid) => Some(windows()?.iter().find(|w|w.process==pid).and_then(|w|w.id.rsplit(':').next())
                .and_then(|v|u64::from_str_radix(v,16).ok()).ok_or("preview process has no eligible window")?),
            None => None,
        };
        let mut hooks=Hooks(Vec::new());
        hooks.add(EVENT_OBJECT_CREATE,EVENT_OBJECT_NAMECHANGE,scope.process.unwrap_or(0))?;
        hooks.add(EVENT_SYSTEM_MINIMIZESTART,EVENT_SYSTEM_MINIMIZEEND,scope.process.unwrap_or(0))?;
        // A different process taking focus clears this scene's focused slot.
        hooks.add(EVENT_SYSTEM_FOREGROUND,EVENT_SYSTEM_FOREGROUND,0)?;
        let waiter=wait::Waiter::new(wake.clone())?;
        Ok(Self { scope,created,device:capture::Device::new(Some(wake))?,slots:(0..max).map(|_|None).collect(),
            send,_hooks:hooks,consumed:None,scale:1.0,waiter,warned:HashSet::new(),focused:None })
    }
    fn tell(&self,event:NestEvent) -> Result<()> { self.send.send(ToRender::Nest(event))?; Ok(()) }
    fn order(&self) -> Result<()> {
        self.tell(NestEvent::Order(self.slots.iter().enumerate()
            .filter_map(|(i,s)|s.as_ref().map(|_|i)).collect()))
    }
    fn focus(&mut self) -> Result<()> {
        let foreground=unsafe { GetForegroundWindow() };
        let root=unsafe { GetAncestor(foreground,GA_ROOTOWNER) };
        let id=Identity::read(root).map(Identity::token);
        let now=self.slots.iter().position(|s|s.as_ref().is_some_and(|s|id.as_ref()==Some(&s.window.id)));
        if now!=self.focused { self.tell(NestEvent::Focused(now))?;self.focused=now; }
        Ok(())
    }
    fn capture(&mut self,i:usize) {
        let existing:u64=self.slots.iter().enumerate().filter(|(k,_)|*k!=i)
            .filter_map(|(_,s)|s.as_ref()).map(Slot::pixels).sum();
        let Some(slot)=self.slots[i].as_mut() else { return; };
        if slot.window.minimized { return; }
        let result=(|| -> Result<_> { let (hwnd,_)=target(&slot.window.id)?;
            Ok(capture::Capture::new(self.device.clone(),hwnd,16_777_216u64.saturating_sub(existing))?) })();
        match result {
            Ok(capture) => { slot.capture=Some(capture);slot.received=false;slot.born=Instant::now();slot.next_frame=Instant::now(); },
            Err(error) => eprintln!("windows preview: could not capture {}: {error}",slot.window.id),
        }
    }
    fn refresh(&mut self) -> Result<()> {
        let screens=monitors()?;
        let Some(source)=screens.iter().find(|m|m.name==self.scope.monitor) else {
            for i in 0..self.slots.len() {
                if self.slots[i].take().is_some() { self.tell(NestEvent::Closed(i))?; }
            }
            self.order()?;
            self.focus()?;
            return Ok(());
        };
        self.scale=source.scale;
        let live:Vec<_>=windows()?.into_iter().filter(|w| w.process!=std::process::id()
            && self.scope.allows(w,self.created)).collect();
        for i in 0..self.slots.len() {
            let remove=self.slots[i].as_ref().is_some_and(|s|!live.iter().any(|w|w.id==s.window.id));
            if remove {
                self.slots[i]=None;
                self.tell(NestEvent::Closed(i))?;
            }
        }
        for window in live {
            if let Some(i)=self.slots.iter().position(|s|s.as_ref().is_some_and(|s|s.window.id==window.id)) {
                let slot=self.slots[i].as_mut().unwrap();
                let title=slot.window.title!=window.title;
                let app=slot.window.app!=window.app;
                let state=slot.window.minimized!=window.minimized;
                let minimized=window.minimized;
                if minimized { slot.capture=None; }
                slot.window=window;
                slot.configure.wake();
                if title { self.tell(NestEvent::Title(i,self.slots[i].as_ref().unwrap().window.title.clone()))?; }
                if app { self.tell(NestEvent::App(i,self.slots[i].as_ref().unwrap().window.app.clone()))?; }
                if state {
                    self.tell(NestEvent::Minimized(i,minimized))?;
                    if !minimized { self.capture(i); }
                }
                continue;
            }
            let Some(i)=self.slots.iter().position(Option::is_none) else { break; };
            self.tell(NestEvent::Opened {slot:i,title:window.title.clone(),app:window.app.clone(),screen:0})?;
            self.tell(NestEvent::Minimized(i,window.minimized))?;
            self.slots[i]=Some(Slot {window,capture:None,received:false,pixels:0,born:Instant::now(),next_frame:Instant::now(),
                configure:configure::Configure::default()});
            self.capture(i);
        }
        self.order()?;
        self.focus()?;
        Ok(())
    }
    fn frames(&mut self) -> Result<()> {
        let scale=self.scale;
        let mut sent=false;
        for i in 0..self.slots.len() {
            let others:u64=self.slots.iter().enumerate().filter(|(k,_)|*k!=i).filter_map(|(_,s)|s.as_ref())
                .map(Slot::pixels).sum();
            let Some(slot)=self.slots[i].as_mut() else { continue; };
            let Some(capture)=slot.capture.as_mut() else { continue; };
            let now=Instant::now();
            if !capture.pending() && ((!capture.ready() && (slot.received || slot.born.elapsed()<Duration::from_secs(5))) || now<slot.next_frame) { continue; }
            // Throttle starting copies, not finishing a copy already on the GPU.
            if !capture.pending() { slot.next_frame=now+Duration::from_secs_f64(1.0/30.0); }
            match capture.next(16_777_216u64.saturating_sub(others)) {
                Ok(Some(picture)) => {
                    slot.received=true;
                    slot.pixels=picture.size.0 as u64*picture.size.1 as u64;
                    let (w,h)=picture.size;
                    let size=((w as f64/scale).round().max(1.0) as u32,(h as f64/scale).round().max(1.0) as u32);
                    self.tell(NestEvent::Frame {slot:i,geometry:[0,0,size.0 as i32,size.1 as i32],pieces:vec![WindowPiece {
                        id:i as u64+1,at:(0,0),size,px:(w,h),src:[0.0,0.0,w as f32,h as f32],content:PieceContent::Pixels(picture.pixels)
                    }]})?;
                    sent=true;
                },
                Ok(None) if slot.received || slot.born.elapsed()<Duration::from_secs(5) => {},
                result => {
                    eprintln!("windows preview: capture stopped for {}: {}",slot.window.id,
                        result.err().map(|e|e.to_string()).unwrap_or_else(||"no capture frame received".into()));
                    slot.capture=None;
                },
            }
        }
        if sent {
            // A query on the same FIFO confirms this batch was consumed. The
            // generic FrameDone message can belong to an earlier repaint.
            let (reply,consumed)=mpsc::channel();
            self.send.send(ToRender::Query("screen.width",reply))?;
            self.consumed=Some(consumed);
        }
        Ok(())
    }
    fn run(&mut self,commands:mpsc::Receiver<ToNest>) -> Result<()> {
        let mut topology=Instant::now();
        loop {
            pump();
            for _ in 0..256 {
                match commands.try_recv() {
                    Ok(ToNest::Quit)|Err(TryRecvError::Disconnected) => return Ok(()),
                    Err(TryRecvError::Empty) => break,
                    Ok(ToNest::FrameDone) => {},
                    Ok(ToNest::Size(..)|ToNest::Shown {..}|ToNest::OnScreen(..)|ToNest::Gpu {..}|ToNest::Released(..)|ToNest::PointerOut|ToNest::HostFocus(..)) => {},
                    Ok(ToNest::Configure {slot,w,h}) => {
                        if !self.scope.actions && (w,h)==(0,0) { continue; }
                        let result=if !self.scope.actions { Err("scene resizing requires --window-actions; this scene is view-only".into()) }
                            else { self.slots.get_mut(slot).and_then(Option::as_mut)
                                .ok_or_else(||Box::<dyn std::error::Error>::from("window slot is no longer open"))
                                .and_then(|slot|slot.configure.ask(w,h)) };
                        if let Err(error)=result { eprintln!("windows preview: {error}"); }
                    },
                    Ok(message @ (ToNest::Focus(_)|ToNest::Close(_)|ToNest::Minimize(..))) => {
                        if let Err(error)=self.action(message) { eprintln!("windows preview: {error}"); }
                        CATALOG_DIRTY.set(true);
                    },
                    Ok(message) => {
                        let kind=match message {
                            ToNest::Pointer {..}|ToNest::Button {..}|ToNest::Wheel(..)|ToNest::Key {..} => "input",
                            _ => "window actions",
                        };
                        if self.warned.insert(kind) { eprintln!("windows preview: {kind} are unavailable; use the original native window"); }
                    },
                }
            }
            if CATALOG_DIRTY.replace(false)||topology.elapsed()>=Duration::from_secs(2) {
                self.refresh()?;topology=Instant::now();
            }
            for i in 0..self.slots.len() {
                if !self.slots[i].as_ref().is_some_and(|s|s.configure.wait(Instant::now()).is_some_and(|d|d.is_zero())) { continue; }
                let others:u64=self.slots.iter().enumerate().filter(|(k,_)|*k!=i).filter_map(|(_,s)|s.as_ref())
                    .map(Slot::pixels).sum();
                if let Some(slot)=self.slots[i].as_mut() {
                    if let Err(error)=slot.configure.tick(&self.scope,self.created,&slot.window.id,16_777_216u64.saturating_sub(others)) {
                        eprintln!("windows preview: {error}");
                    }
                }
            }
            if let Some(consumed)=&self.consumed {
                match consumed.try_recv() {
                    Ok(_) => self.consumed=None,
                    Err(TryRecvError::Disconnected) => return Err("render frame acknowledgement closed".into()),
                    Err(TryRecvError::Empty) => {},
                }
            }
            if self.consumed.is_none() { self.frames()?; }
            let now=Instant::now();
            let mut wait=(topology+Duration::from_secs(2)).saturating_duration_since(now);
            for slot in self.slots.iter().flatten() {
                if let Some(resize)=slot.configure.wait(now) { wait=wait.min(resize); }
            }
            if self.consumed.is_some() { wait=wait.min(Duration::from_millis(2)); }
            else {
                for slot in self.slots.iter().flatten() {
                    let Some(capture)=&slot.capture else { continue; };
                    if capture.pending() { wait=wait.min(Duration::from_millis(2)); }
                    else if capture.ready() { wait=wait.min(slot.next_frame.saturating_duration_since(now)); }
                }
            }
            self.waiter.wait(wait)?;
        }
    }
    fn action(&self,message:ToNest) -> Result<()> {
        let (i,action)=match message {
            ToNest::Focus(i)=>(i,Action::Focus),
            ToNest::Close(i)=>(i,Action::Close),
            ToNest::Minimize(i,yes)=>(i,Action::Minimize(yes)),
            _=>return Err("unsupported native window action".into()),
        };
        let slot=self.slots.get(i).and_then(Option::as_ref).ok_or("window slot is no longer open")?;
        act(&self.scope,self.created,&slot.window.id,action)
    }
}

enum Action { Focus, Close, Minimize(bool) }
fn act(scope:&Scope,created:Option<u64>,id:&str,action:Action) -> Result<()> {
    if !scope.actions { return Err("window actions require --window-actions; this scene is view-only".into()); }
    let (hwnd,window)=target(id)?;
    if !scope.allows(&window,created) { return Err("window left the selected monitor or process scope".into()); }
    match action {
        Action::Minimize(yes) => { state(id,yes)?; },
        Action::Close => {
            // An application may cancel closing or show an unsaved-work dialog.
            // Only its actual disappearance removes its slot.
            unsafe { PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0)) }?;
        },
        Action::Focus => {
            if window.minimized { state(id,false)?; }
            let (hwnd,window)=target(id)?;
            if !scope.allows(&window,created) { return Err("window left the selected scope while restoring".into()); }
            if !unsafe { SetForegroundWindow(hwnd) }.as_bool() {
                return Err("Windows denied foreground activation; select the scene and try again".into());
            }
        },
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_actions_need_explicit_opt_in() {
        let scope=Scope {monitor:"unused".into(),process:None,actions:false};
        for action in [Action::Focus,Action::Close,Action::Minimize(true),Action::Minimize(false)] {
            assert!(act(&scope,None,"invalid",action).unwrap_err().to_string().contains("--window-actions"));
        }
        assert!(prepare(&["--window-actions".into()]).is_err());
    }
    #[test]
    fn actions_remain_scoped_after_catalog_changes() {
        let scope=Scope {monitor:"secondary".into(),process:Some(12),actions:true};
        let mut window=Window {id:"12:13:a:ff".into(),title:"fixture".into(),app:"fixture.exe".into(),
            class:"fixture".into(),process:12,monitor:scope.monitor.clone(),
            bounds:Bounds {x:0,y:0,width:400,height:300},minimized:true,maximized:false,resizable:true};
        assert!(scope.allows(&window,Some(255)));
        assert!(!scope.allows(&window,Some(256)));
        window.process=13;assert!(!scope.allows(&window,Some(255)));
        window.process=12;window.monitor="primary".into();assert!(!scope.allows(&window,Some(255)));
        let slot=Slot {window,capture:None,received:true,pixels:1_000_000,born:Instant::now(),next_frame:Instant::now(),
            configure:configure::Configure::default()};
        assert_eq!(slot.pixels(),1_000_000,"suspended capture must retain its renderer memory budget");
    }
}
