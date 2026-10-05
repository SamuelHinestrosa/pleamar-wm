//! Integration with a separate real daemon. Only this process's test HWNDs
//! on a verified secondary display are ever managed.
use super::*;
use super::tests::{OwnWindows, ThreadDpi, fixture_proc};
use std::{os::windows::process::CommandExt, process::{Child, Command, Stdio}, sync::mpsc};

struct OwnedDaemon(Child);
impl Drop for OwnedDaemon { fn drop(&mut self) { let _=self.0.kill(); let _=self.0.wait(); } }

fn request(endpoint:&ipc::Endpoint, line:&str) -> Result<Value> {
    let endpoint=endpoint.clone();
    let line=line.to_owned();
    let (send, receive)=mpsc::channel();
    let task=std::thread::spawn(move || { let _=send.send(endpoint.ask(&line).map_err(|e|e.to_string())); });
    let result=loop {
        pump();
        match receive.try_recv() {
            Ok(result)=>break result,
            Err(mpsc::TryRecvError::Empty)=>std::thread::sleep(Duration::from_millis(5)),
            Err(e)=>return Err(e.into()),
        }
    };
    task.join().map_err(|_|"WM client thread panicked")?;
    result.map_err(Into::into)
}

fn until(mut check:impl FnMut()->bool) {
    let start=Instant::now();
    loop { pump(); if check() { break; } assert!(start.elapsed()<Duration::from_secs(10),"native WM state timed out"); std::thread::sleep(Duration::from_millis(15)); }
}

fn resources(pid:u32) -> Result<(u64,usize,usize)> {
    use windows::Win32::System::ProcessStatus::*;
    let handle=unsafe { OpenProcess(PROCESS_QUERY_INFORMATION|PROCESS_VM_READ,false,pid) }?;
    let result=(||->Result<_> {
        let mut created=FILETIME::default(); let mut exit=FILETIME::default();
        let mut kernel=FILETIME::default(); let mut user=FILETIME::default();
        unsafe { GetProcessTimes(handle,&mut created,&mut exit,&mut kernel,&mut user) }?;
        let mut memory=PROCESS_MEMORY_COUNTERS_EX::default();
        memory.cb=size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        unsafe { GetProcessMemoryInfo(handle,&mut memory as *mut _ as _,memory.cb) }?;
        let ticks=|v:FILETIME| ((v.dwHighDateTime as u64)<<32)|v.dwLowDateTime as u64;
        Ok((ticks(kernel)+ticks(user),memory.WorkingSetSize,memory.PrivateUsage))
    })();
    let _=unsafe { CloseHandle(handle) };
    result
}

#[test]
#[ignore = "starts a scoped daemon and its own test windows on a non-primary monitor"]
fn native_session_lifecycle() -> Result<()> {
    let requested=std::env::var("PLEAMAR_WM_TEST_MONITOR")?;
    if !requested.starts_with(r"\\.\DISPLAY") { return Err("explicit secondary monitor required".into()); }
    let dpi=unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    assert!(!dpi.0.is_null());
    let _dpi=ThreadDpi(dpi);
    let monitor=select_monitor(&requested)?;
    assert!(!monitor.primary);
    let foreground=unsafe { GetForegroundWindow() };
    let module=unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }?;
    let class=WNDCLASSW { lpfnWndProc:Some(fixture_proc), hInstance:module.into(), lpszClassName:w!("pleamar-wm-session-fixture"), ..Default::default() };
    assert_ne!(unsafe { RegisterClassW(&class) },0);
    let mut owned=OwnWindows(Vec::new());
    let mut originals=Vec::new();
    let create=|i:i32|->Result<HWND> {
        let rect=Bounds { x:monitor.work.x+40+i*30,y:monitor.work.y+50+i*30,width:400,height:280 };
        assert!(monitor.work.contains(&rect));
        let title:Vec<u16>=format!("pleamar-wm session test ñ {i}").encode_utf16().chain([0]).collect();
        let hwnd=unsafe { CreateWindowExW(WS_EX_APPWINDOW,class.lpszClassName,PCWSTR(title.as_ptr()),WS_OVERLAPPEDWINDOW,
            rect.x,rect.y,rect.width,rect.height,None,None,Some(module.into()),None) }?;
        let _=unsafe { ShowWindow(hwnd,SW_SHOWNOACTIVATE) };
        pump();
        Ok(hwnd)
    };
    for i in 0..3 {
        let hwnd=create(i)?;
        owned.0.push(hwnd);
        let w=inspect(hwnd).unwrap();
        originals.push(SavedWindow { id:w.id,bounds:w.bounds });
    }
    let nonce=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH)?.as_nanos();
    let out=std::env::var_os("PLEAMAR_WM_TEST_OUTPUT").map(std::path::PathBuf::from)
        .unwrap_or_else(||std::env::temp_dir().join(format!("wm session ñ {nonce}")));
    std::fs::create_dir(&out)?;
    let state_file=out.join("session ñ.json");
    let namespace=format!("test-{}-{nonce}",std::process::id());
    let endpoint=ipc::Endpoint::new(&namespace)?;
    let binary=std::path::PathBuf::from(std::env::var_os("PLEAMAR_WM_TEST_BINARY").ok_or("set PLEAMAR_WM_TEST_BINARY")?).canonicalize()?;
    let host=std::env::var_os("PLEAMAR_WM_TEST_HOST").map(std::path::PathBuf::from);
    let start=|round:usize|->Result<OwnedDaemon> {
        let log=std::fs::File::create(out.join(format!("daemon-{round}.log")))?;
        let mut command=Command::new(host.as_ref().unwrap_or(&binary));
        if host.is_none() { command.arg("session"); }
        let child=command.args(["--monitor",&requested,"--process",&std::process::id().to_string(),
            "--namespace",&namespace,"--state",state_file.to_str().unwrap(),"--seconds","120"])
            .stdout(log.try_clone()?).stderr(Stdio::from(log)).creation_flags(0x08000000|0x00004000).spawn()?;
        Ok(OwnedDaemon(child))
    };
    let mut daemon=start(1)?;
    until(|| { assert!(daemon.0.try_wait().unwrap().is_none()); request(&endpoint,"status").is_ok() });
    let status=request(&endpoint,"status")?;
    assert_eq!(status["monitors"][0]["tiled"],false);
    for old in &originals { assert_eq!(target(&old.id)?.1.bounds,old.bounds); }
    request(&endpoint,&format!("layout {requested} grid"))?;
    assert_eq!(request(&endpoint,"status")?["saved_windows"],3);
    for old in &originals { assert!(monitor.work.contains(&target(&old.id)?.1.bounds)); }
    let added=create(3)?;
    owned.0.push(added);
    until(||request(&endpoint,"status").unwrap()["monitors"][0]["windows"]==4);
    let _=unsafe { DestroyWindow(owned.0.pop().unwrap()) };
    until(||request(&endpoint,"status").unwrap()["monitors"][0]["windows"]==3);
    state(&originals[0].id,true)?;
    until(||request(&endpoint,"status").unwrap()["monitors"][0]["windows"]==2);
    request(&endpoint,&format!("free {requested}"))?;
    assert!(target(&originals[0].id)?.1.minimized);
    assert_eq!(request(&endpoint,"status")?["pending_recovery"],0);
    state(&originals[0].id,false)?;
    for old in &originals { assert_eq!(target(&old.id)?.1.bounds,old.bounds); }
    unsafe { SetWindowLongPtrW(owned.0[1],GWLP_USERDATA,1); }
    assert!(request(&endpoint,&format!("layout {requested} columns")).is_err());
    let failed=request(&endpoint,"status")?;
    assert_eq!(failed["monitors"][0]["tiled"],false);
    assert!(failed["monitors"][0]["error"].is_string());
    for old in &originals { assert_eq!(target(&old.id)?.1.bounds,old.bounds); }
    unsafe { SetWindowLongPtrW(owned.0[1],GWLP_USERDATA,0); }
    request(&endpoint,&format!("layout {requested} rows"))?;
    daemon.0.kill()?;
    daemon.0.wait()?;
    drop(daemon);
    let mut daemon=start(2)?;
    until(|| { assert!(daemon.0.try_wait().unwrap().is_none()); request(&endpoint,"status").is_ok() });
    for old in &originals { assert_eq!(target(&old.id)?.1.bounds,old.bounds); }
    let settled=Instant::now();
    while settled.elapsed()<Duration::from_millis(400) { pump(); std::thread::sleep(Duration::from_millis(10)); }
    let before=request(&endpoint,"status")?;
    let used_before=resources(daemon.0.id())?;
    let idle=Instant::now();
    while idle.elapsed()<Duration::from_secs(5) { pump(); std::thread::sleep(Duration::from_millis(25)); }
    let after=request(&endpoint,"status")?;
    let elapsed=idle.elapsed().as_secs_f64();
    let used_after=resources(daemon.0.id())?;
    assert_eq!(after["catalog_scans"],before["catalog_scans"],"idle WM repeatedly scanned the desktop");
    assert_eq!(after["geometry_changes"],before["geometry_changes"]);
    assert!(request(&endpoint,"emit toggle_rain").is_err());
    request(&endpoint,&format!("layout {requested} left"))?;
    let done=request(&endpoint,"quit")?;
    assert_eq!(done["pending_recovery"],0);
    until(||daemon.0.try_wait().unwrap().is_some());
    assert!(daemon.0.wait()?.success());
    for old in &originals { assert_eq!(target(&old.id)?.1.bounds,old.bounds); }
    let focus_unchanged=unsafe { GetForegroundWindow() }==foreground;
    assert!(focus_unchanged);
    drop(owned);
    unsafe { UnregisterClassW(class.lpszClassName,Some(module.into())) }?;
    let report=json!({"passed":true,"monitor":requested,"primary":false,"physical_input":false,
        "focus_unchanged":focus_unchanged,"automatic_create_close":true,"minimize_restore":true,
        "free_while_minimized":true,"resize_rejection_rollback":true,"crash_recovery":true,
        "unicode_journal":true,"idle_seconds":5,"idle_catalog_scans":0,"quit_restores":true});
    let mut report=report;
    report["idle_one_core_cpu_percent"]=json!((used_after.0-used_before.0) as f64/10000000.0/elapsed*100.0);
    report["daemon_working_set_bytes"]=json!(used_after.1);
    report["daemon_private_commit_bytes"]=json!(used_after.2);
    report["gui_host"]=json!(host.is_some());
    std::fs::write(out.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    println!("{report}");
    Ok(())
}
