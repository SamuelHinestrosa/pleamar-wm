//! Live native window pictures for the scene renderer. This explicit preview
//! mode never redirects input or changes the source applications' geometry.
use super::*;
use pleamar::scene::{NestEvent, PieceContent, ToNest, ToRender, WindowPiece};
use std::{cell::Cell, sync::{OnceLock, mpsc::{self, Sender, TryRecvError}}};
use windows::Win32::UI::Accessibility::*;

#[derive(Clone)]
struct Scope { monitor:String, process:Option<u32> }
static SCOPE:OnceLock<Scope> = OnceLock::new();
thread_local! { static CATALOG_DIRTY:Cell<bool> = const { Cell::new(true) }; }
unsafe extern "system" fn changed(_:HWINEVENTHOOK,_:u32,_:HWND,object:i32,child:i32,_:u32,_:u32) {
    if object == 0 && child == 0 { CATALOG_DIRTY.set(true); }
}
struct Hooks(HWINEVENTHOOK);
impl Drop for Hooks { fn drop(&mut self) { let _=unsafe { UnhookWinEvent(self.0) }; } }

pub(super) fn prepare(args:&[String]) -> Result<Vec<String>> {
    let mut monitor=std::env::var("PLEAMAR_WM_PREVIEW_MONITOR").ok();
    let mut process=std::env::var("PLEAMAR_WM_PREVIEW_PROCESS").ok();
    let mut forwarded=Vec::new(); let mut it=args.iter();
    while let Some(argument)=it.next() {
        match argument.as_str() {
            "--preview-monitor" => monitor=Some(it.next().ok_or("preview monitor missing")?.clone()),
            "--preview-process" => process=Some(it.next().ok_or("preview process missing")?.clone()),
            _ => forwarded.push(argument.clone()),
        }
    }
    let Some(monitor)=monitor else {
        if process.is_some() { return Err("a preview process needs an explicit preview monitor".into()); }
        return Ok(forwarded);
    };
    let monitor=select_monitor(&monitor)?.name;
    let process=process.map(|v|v.parse::<u32>()).transpose()?;
    if process==Some(0) || process==Some(std::process::id()) { return Err("invalid preview process".into()); }
    SCOPE.set(Scope { monitor,process }).map_err(|_|"window preview is already configured")?;
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

struct Slot { window:Window, capture:capture::Capture, announced:bool, born:Instant, next_frame:Instant }
struct Preview {
    scope:Scope, created:Option<u64>, device:std::rc::Rc<capture::Device>,
    slots:Vec<Option<Slot>>, send:Sender<ToRender>, _hooks:Hooks,
    consumed:Option<mpsc::Receiver<String>>, scale:f64, waiter:wait::Waiter, warned:HashSet<&'static str>,
}
impl Preview {
    fn new(max:usize,scope:Scope,send:Sender<ToRender>,wake:std::sync::Arc<wait::Wake>) -> Result<Self> {
        if !(1..=64).contains(&max) { return Err("invalid native window slot count".into()); }
        let created=match scope.process {
            Some(pid) => Some(windows()?.iter().find(|w|w.process==pid).and_then(|w|w.id.rsplit(':').next())
                .and_then(|v|u64::from_str_radix(v,16).ok()).ok_or("preview process has no eligible window")?),
            None => None,
        };
        let hook=unsafe { SetWinEventHook(EVENT_OBJECT_CREATE,EVENT_OBJECT_NAMECHANGE,None,Some(changed),
            scope.process.unwrap_or(0),0,WINEVENT_OUTOFCONTEXT|WINEVENT_SKIPOWNPROCESS) };
        if hook.is_invalid() { return Err("native window event subscription failed".into()); }
        let hooks=Hooks(hook);
        let waiter=wait::Waiter::new(wake.clone())?;
        Ok(Self { scope,created,device:capture::Device::new(Some(wake))?,slots:(0..max).map(|_|None).collect(),
            send,_hooks:hooks,consumed:None,scale:1.0,waiter,warned:HashSet::new() })
    }
    fn tell(&self,event:NestEvent) -> Result<()> { self.send.send(ToRender::Nest(event))?; Ok(()) }
    fn order(&self) -> Result<()> {
        self.tell(NestEvent::Order(self.slots.iter().enumerate()
            .filter_map(|(i,s)|s.as_ref().filter(|s|s.announced).map(|_|i)).collect()))
    }
    fn refresh(&mut self) -> Result<()> {
        let screens=monitors()?;
        let Some(source)=screens.iter().find(|m|m.name==self.scope.monitor) else {
            for i in 0..self.slots.len() {
                if self.slots[i].take().is_some_and(|s|s.announced) { self.tell(NestEvent::Closed(i))?; }
            }
            self.order()?;
            return Ok(());
        };
        self.scale=source.scale;
        let live:Vec<_>=windows()?.into_iter().filter(|w| w.monitor==self.scope.monitor
            && w.process!=std::process::id() && !w.minimized
            && self.scope.process.is_none_or(|pid|w.process==pid)
            && self.created.is_none_or(|created|w.id.ends_with(&format!(":{created:x}")))).collect();
        for i in 0..self.slots.len() {
            let remove=self.slots[i].as_ref().is_some_and(|s|s.capture.closed()||!live.iter().any(|w|w.id==s.window.id));
            if remove {
                let slot=self.slots[i].take().unwrap();
                if slot.announced { self.tell(NestEvent::Closed(i))?; }
            }
        }
        for window in live {
            if let Some(i)=self.slots.iter().position(|s|s.as_ref().is_some_and(|s|s.window.id==window.id)) {
                let slot=self.slots[i].as_mut().unwrap();
                let changed=slot.announced && slot.window.title!=window.title;
                slot.window=window;
                if changed { self.tell(NestEvent::Title(i,self.slots[i].as_ref().unwrap().window.title.clone()))?; }
                continue;
            }
            let Some(i)=self.slots.iter().position(Option::is_none) else { break; };
            let (hwnd,_)=target(&window.id)?;
            let existing:u64=self.slots.iter().flatten().map(|s|s.capture.size().map(|(w,h)|w as u64*h as u64).unwrap_or(0)).sum();
            match capture::Capture::new(self.device.clone(),hwnd,16_777_216u64.saturating_sub(existing)) {
                Ok(capture) => {
                    self.slots[i]=Some(Slot {window,capture,announced:false,born:Instant::now(),next_frame:Instant::now()});
                },
                Err(error) => eprintln!("windows preview: could not capture {}: {error}",window.id),
            }
        }
        self.order()?;
        Ok(())
    }
    fn frames(&mut self) -> Result<()> {
        let scale=self.scale;
        let mut sent=false;
        let mut changed=false;
        for i in 0..self.slots.len() {
            let others:u64=self.slots.iter().enumerate().filter(|(k,_)|*k!=i).filter_map(|(_,s)|s.as_ref())
                .map(|s|s.capture.size().map(|(w,h)|w as u64*h as u64).unwrap_or(0)).sum();
            let Some(slot)=self.slots[i].as_mut() else { continue; };
            let now=Instant::now();
            if !slot.capture.pending() && ((!slot.capture.ready() && (slot.announced || slot.born.elapsed()<Duration::from_secs(5))) || now<slot.next_frame) { continue; }
            // Throttle starting copies, not finishing a copy already on the GPU.
            if !slot.capture.pending() { slot.next_frame=now+Duration::from_secs_f64(1.0/30.0); }
            match slot.capture.next(16_777_216u64.saturating_sub(others)) {
                Ok(Some(picture)) => {
                    let title=slot.window.title.clone();let app=slot.window.class.clone();
                    let announced=slot.announced;slot.announced=true;
                    if !announced { self.tell(NestEvent::Opened {slot:i,title,app,screen:0})?; changed=true; }
                    let (w,h)=picture.size;
                    let size=((w as f64/scale).round().max(1.0) as u32,(h as f64/scale).round().max(1.0) as u32);
                    self.tell(NestEvent::Frame {slot:i,geometry:[0,0,size.0 as i32,size.1 as i32],pieces:vec![WindowPiece {
                        id:i as u64+1,at:(0,0),size,px:(w,h),src:[0.0,0.0,w as f32,h as f32],content:PieceContent::Pixels(picture.pixels)
                    }]})?;
                    sent=true;
                },
                Ok(None) if slot.announced || slot.born.elapsed()<Duration::from_secs(5) => {},
                result => {
                    eprintln!("windows preview: capture stopped for {}: {}",slot.window.id,
                        result.err().map(|e|e.to_string()).unwrap_or_else(||"no capture frame received".into()));
                    let announced=slot.announced;self.slots[i]=None;
                    if announced { self.tell(NestEvent::Closed(i))?; changed=true; }
                },
            }
        }
        if changed { self.order()?; }
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
                    Ok(message) => {
                        let kind=match message {
                            ToNest::Pointer {..}|ToNest::Button {..}|ToNest::Wheel(..)|ToNest::Key {..} => "input",
                            _ => "window actions",
                        };
                        if self.warned.insert(kind) { eprintln!("windows preview: {kind} are unavailable in view-only mode"); }
                    },
                }
            }
            if CATALOG_DIRTY.replace(false)||topology.elapsed()>=Duration::from_secs(2) {
                self.refresh()?;topology=Instant::now();
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
            if self.consumed.is_some() { wait=wait.min(Duration::from_millis(2)); }
            else {
                for slot in self.slots.iter().flatten() {
                    if slot.capture.pending() { wait=wait.min(Duration::from_millis(2)); }
                    else if slot.capture.ready() { wait=wait.min(slot.next_frame.saturating_duration_since(now)); }
                }
            }
            self.waiter.wait(wait)?;
        }
    }
}
