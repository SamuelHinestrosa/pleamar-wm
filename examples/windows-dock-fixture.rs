//! Owned non-activating app for dock launch/file-argument acceptance.
#[cfg(not(target_os="windows"))]
fn main() { eprintln!("this fixture requires Windows"); }
#[cfg(target_os="windows")]
fn main() {
    if let Err(error)=fixture::run() {eprintln!("{error}");std::process::exit(1);}
}
#[cfg(target_os="windows")]
mod fixture {
    use windows::{core::{w,BOOL},Win32::{Foundation::*,Graphics::Gdi::*,System::LibraryLoader::*,UI::{HiDpi::*,WindowsAndMessaging::*}}};
    use std::{mem::size_of,path::PathBuf};
    struct Find {name:String,work:Option<RECT>}
    unsafe extern "system" fn monitor(handle:HMONITOR,_:HDC,_:*mut RECT,data:LPARAM) -> BOOL {
        let find=unsafe {&mut *(data.0 as *mut Find)};
        let mut info=MONITORINFOEXW::default();info.monitorInfo.cbSize=size_of::<MONITORINFOEXW>() as u32;
        if unsafe {GetMonitorInfoW(handle,&mut info.monitorInfo)}.as_bool() {
            let name=String::from_utf16_lossy(&info.szDevice[..info.szDevice.iter().position(|c|*c==0).unwrap_or(info.szDevice.len())]);
            if name==find.name && info.monitorInfo.dwFlags&MONITORINFOF_PRIMARY==0 {find.work=Some(info.monitorInfo.rcWork);}
        }
        true.into()
    }
    unsafe extern "system" fn window(hwnd:HWND,message:u32,w:WPARAM,l:LPARAM) -> LRESULT {
        unsafe {
            match message {
                WM_PAINT=>{
                    let mut paint=PAINTSTRUCT::default();let dc=BeginPaint(hwnd,&mut paint);
                    let mut area=RECT {left:18,top:18,right:300,bottom:150};
                    let mut text:Vec<u16>="Owned dock application — ñ 海\nNo keyboard or mouse input.".encode_utf16().collect();
                    DrawTextW(dc,&mut text,&mut area,DT_LEFT|DT_WORDBREAK);let _=EndPaint(hwnd,&paint);LRESULT(0)
                },
                WM_TIMER=>{let _=DestroyWindow(hwnd);LRESULT(0)},
                WM_DESTROY=>{PostQuitMessage(0);LRESULT(0)},
                _=>DefWindowProcW(hwnd,message,w,l),
            }
        }
    }
    pub fn run() -> Result<(),Box<dyn std::error::Error>> {
        let name=std::env::var("PLEAMAR_DOCK_FIXTURE_MONITOR")?;
        let root=PathBuf::from(std::env::var_os("PLEAMAR_DOCK_FIXTURE_ROOT").ok_or("missing fixture output directory")?);
        if !root.is_absolute() || !root.is_dir() {return Err("fixture output must be an existing absolute directory".into());}
        unsafe {SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2)}?;
        let mut find=Find {name,work:None};
        if !unsafe {EnumDisplayMonitors(None,None,Some(monitor),LPARAM(&mut find as *mut _ as isize))}.as_bool() {return Err("monitor enumeration failed".into());}
        let area=find.work.ok_or("the explicit non-primary fixture monitor is unavailable")?;
        if area.right-area.left<400 || area.bottom-area.top<260 {return Err("secondary work area is too small".into());}
        let module=unsafe {GetModuleHandleW(None)}?;
        let class=WNDCLASSW {lpfnWndProc:Some(window),hInstance:module.into(),lpszClassName:w!("pleamar-owned-dock-app"),
            hbrBackground:unsafe {HBRUSH(GetStockObject(WHITE_BRUSH).0)},..Default::default()};
        if unsafe {RegisterClassW(&class)}==0 {return Err("fixture registration failed".into());}
        let hwnd=unsafe {CreateWindowExW(WS_EX_APPWINDOW,class.lpszClassName,w!("Owned dock app ñ"),WS_OVERLAPPEDWINDOW,
            area.left+24,area.top+24,340,220,None,None,Some(module.into()),None)}?;
        unsafe {let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);SetTimer(Some(hwnd),1,60_000,None);}
        let report=serde_json::json!({"pid":std::process::id(),"hwnd":hwnd.0 as usize,"monitor":find.name,"args":std::env::args().skip(1).collect::<Vec<_>>()});
        use std::io::Write;
        let mut file=std::fs::OpenOptions::new().write(true).create_new(true).open(root.join(format!("launch-{}.json",std::process::id())))?;
        file.write_all(serde_json::to_string_pretty(&report)?.as_bytes())?;drop(file);
        let mut message=MSG::default();
        while unsafe {GetMessageW(&mut message,None,0,0)}.0>0 {unsafe {let _=TranslateMessage(&message);DispatchMessageW(&message);}}
        Ok(())
    }
}
