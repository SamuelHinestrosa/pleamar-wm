//! A native renderer regression: a static preview must repaint when only the
//! captured program's pixels change. No synthetic input or third-party window.
use super::*;
use super::tests::{OwnWindows, ThreadDpi};
use std::{os::windows::process::CommandExt, process::{Child, Command, Stdio}};

struct Scene(Child);
impl Drop for Scene { fn drop(&mut self) { let _=self.0.kill();let _=self.0.wait(); } }

fn canvas(pid:u32,monitor:&Monitor) -> Result<Option<HWND>> {
    struct Find { pid:u32,window:Option<HWND> }
    unsafe extern "system" fn visit(hwnd:HWND,data:LPARAM) -> BOOL { unsafe {
        let find=&mut *(data.0 as *mut Find);
        let mut pid=0;GetWindowThreadProcessId(hwnd,Some(&mut pid));
        if pid==find.pid&&IsWindowVisible(hwnd).as_bool() { find.window=Some(hwnd); }
        true.into()
    } }
    let mut find=Find {pid,window:None};
    unsafe { EnumWindows(Some(visit),LPARAM(&mut find as *mut _ as isize)) }?;
    if let Some(hwnd)=find.window {
        let mut rect=RECT::default();unsafe { GetWindowRect(hwnd,&mut rect) }?;
        assert!(monitor.work.contains(&rect.into()),"owned preview left its secondary work area");
    }
    Ok(find.window)
}

fn picture(capture:&mut capture::Capture, scene:&mut Scene,color:[u8;4],present:bool) -> Result<()> {
    let start=Instant::now();
    loop {
        pump();assert!(scene.0.try_wait()?.is_none(),"owned scene exited early");
        if let Some(picture)=capture.next(16_777_216)? {
            let count=picture.pixels.chunks_exact(4).filter(|p|*p==color).count();
            if (present&&count>10_000)||(!present&&count==0) { return Ok(()); }
        }
        if start.elapsed()>Duration::from_secs(10) { return Err("native preview did not repaint its actual source pixels".into()); }
        std::thread::sleep(Duration::from_millis(15));
    }
}

#[test]
#[ignore = "renders and captures only an owned source and scene on an explicit secondary monitor"]
fn native_window_preview_repaints() -> Result<()> {
    let requested=std::env::var("PLEAMAR_WM_TEST_MONITOR")?;
    let binary=std::fs::canonicalize(std::env::var_os("PLEAMAR_WM_TEST_BINARY").ok_or("set PLEAMAR_WM_TEST_BINARY")?)?;
    let dpi=unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!dpi.0.is_null());let _dpi=ThreadDpi(dpi);
    let monitor=select_monitor(&requested)?;assert!(!monitor.primary);
    let foreground=unsafe { GetForegroundWindow() };
    let module=unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }?;
    let class=WNDCLASSW {lpfnWndProc:Some(super::capture_tests::paint_fixture),hInstance:module.into(),
        lpszClassName:w!("pleamar-wm-preview-regression"),..Default::default()};
    assert_ne!(unsafe { RegisterClassW(&class) },0);
    let mut owned=OwnWindows(Vec::new());
    let source=unsafe { CreateWindowExW(WS_EX_APPWINDOW,class.lpszClassName,w!("Owned preview source ñ"),WS_OVERLAPPEDWINDOW,
        monitor.work.x+30,monitor.work.y+40,400,280,None,None,Some(module.into()),None) }?;
    owned.0.push(source);
    let mut rect=RECT::default();unsafe { GetWindowRect(source,&mut rect) }?;
    assert!(monitor.work.contains(&rect.into()));
    unsafe { SetWindowLongPtrW(source,GWLP_USERDATA,0x0020c060);let _=ShowWindow(source,SW_SHOWNOACTIVATE); }
    pump();
    let nonce=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let directory=std::env::temp_dir().join(format!("pleamar-wm preview ñ {} {nonce}",std::process::id()));
    std::fs::create_dir(&directory)?;
    let path=directory.join("native-preview.plm");
    std::fs::write(&path,r##"scene PreviewRegression {
        surface { size: 640, 400; anchor: center; keyboard: none; reserve: 0 }
        windows win max 1
        box { from: 0, 0; size: 640, 400; color: #101820 }
        window win.0 { at: 40, 40; size: 560, 320; ask: -1, -1; show: win.0.open }
    }"##)?;
    let log=std::fs::File::create(directory.join("scene.log"))?;
    let mut scene=Scene(Command::new(binary).arg("--scene").arg(path).args(["--screen",&requested,
        "--preview-monitor",&requested,"--preview-process",&std::process::id().to_string(),
        "--no-hud","--stall","0","--seconds","45"])
        .env("PLEAMAR_TEST_WINDOWS","1").env("PLEAMAR_NO_RELAUNCH","1")
        .env("PLEAMAR_SOCKET_DIR",format!("wm-preview-{nonce}"))
        .stdout(log.try_clone()?).stderr(Stdio::from(log)).creation_flags(0x08000000|0x00004000).spawn()?);
    let start=Instant::now();
    let canvas=loop {
        pump();assert!(scene.0.try_wait()?.is_none());
        if let Some(hwnd)=canvas(scene.0.id(),&monitor)? { break hwnd; }
        assert!(start.elapsed()<Duration::from_secs(12),"native scene never appeared");
        std::thread::sleep(Duration::from_millis(15));
    };
    let mut capture=capture::Capture::new(capture::Device::new(None)?,canvas,16_777_216)?;
    picture(&mut capture,&mut scene,[0x20,0xc0,0x60,255],true)?;
    unsafe { SetWindowLongPtrW(source,GWLP_USERDATA,0x00d03080);let _=InvalidateRect(Some(source),None,false); }
    picture(&mut capture,&mut scene,[0xd0,0x30,0x80,255],true)?;
    unsafe { DestroyWindow(source) }?;owned.0.clear();
    picture(&mut capture,&mut scene,[0xd0,0x30,0x80,255],false)?;
    drop(capture);
    unsafe { PostMessageW(Some(canvas),WM_CLOSE,WPARAM(0),LPARAM(0)) }?;
    let start=Instant::now();
    while scene.0.try_wait()?.is_none()&&start.elapsed()<Duration::from_secs(10) {
        pump();std::thread::sleep(Duration::from_millis(15));
    }
    assert_eq!(scene.0.try_wait()?.and_then(|s|s.code()),Some(0));
    unsafe { UnregisterClassW(class.lpszClassName,Some(module.into())) }?;
    let focus_unchanged=unsafe { GetForegroundWindow() }==foreground;
    println!("{}",json!({"native_window_preview":true,"actual_pixels_changed":true,"source_close":true,
        "view_only":true,"physical_input":false,"focus_unchanged":focus_unchanged,"monitor":requested,"log":directory}));
    assert!(focus_unchanged);Ok(())
}
