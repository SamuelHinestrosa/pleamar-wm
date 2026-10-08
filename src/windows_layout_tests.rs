//! Compare native acknowledgements across independent application UI threads.
use super::*;

unsafe extern "system" fn procedure(hwnd:HWND,message:u32,w:WPARAM,l:LPARAM) -> LRESULT {
    if message==WM_WINDOWPOSCHANGING && unsafe { GetWindowLongPtrW(hwnd,GWLP_USERDATA) }==1 {
        std::thread::sleep(Duration::from_millis(40));
    }
    if message==WM_DESTROY { unsafe { PostQuitMessage(0); } }
    unsafe { DefWindowProcW(hwnd,message,w,l) }
}

struct Applications(Vec<(isize,std::thread::JoinHandle<()>)>);
impl Drop for Applications {
    fn drop(&mut self) {
        for (hwnd,_) in &self.0 { let _=unsafe { PostMessageW(Some(HWND(*hwnd as _)),WM_CLOSE,WPARAM(0),LPARAM(0)) }; }
        for (_,thread) in self.0.drain(..) { let _=thread.join(); }
    }
}

#[test]
#[ignore = "shows only owned windows on a disposable CI desktop"]
fn native_layout_batch_and_drag() -> Result<()> {
    if std::env::var("GITHUB_ACTIONS").as_deref()!=Ok("true")
        || std::env::var("RUNNER_ENVIRONMENT").as_deref()!=Ok("github-hosted")
        || std::env::var("PLEAMAR_WM_CI_LAYOUT").as_deref()!=Ok("1") {
        return Err("requires the explicit layout step on a disposable GitHub-hosted runner".into());
    }
    let out=PathBuf::from(std::env::var_os("PLEAMAR_WM_CI_OUTPUT").ok_or("missing evidence path")?);
    let temp=std::fs::canonicalize(std::env::var_os("RUNNER_TEMP").ok_or("missing runner temp")?)?;
    if std::fs::canonicalize(out.parent().ok_or("invalid evidence path")?)?!=temp || out.exists() {
        return Err("evidence must be a new direct child of RUNNER_TEMP".into());
    }
    std::fs::create_dir(&out)?;
    let previous=unsafe { SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) };
    let _dpi=super::super::tests::ThreadDpi(previous);
    let screen=monitors()?.into_iter().find(|m|m.primary).ok_or("missing CI output")?;
    let module=unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }?;
    let class=WNDCLASSW {lpfnWndProc:Some(procedure),hInstance:module.into(),
        lpszClassName:w!("pleamar-layout-ci"),..Default::default()};
    assert_ne!(unsafe { RegisterClassW(&class) },0);
    let mut apps=Applications(Vec::new());
    let mut ids=Vec::new();
    for i in 0..4 {
        let (send,receive)=std::sync::mpsc::channel();
        let area=screen.work.clone();
        let thread=std::thread::spawn(move|| unsafe {
            let _=SetThreadDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
            let module=windows::Win32::System::LibraryLoader::GetModuleHandleW(None).unwrap();
            let hwnd=CreateWindowExW(WS_EX_APPWINDOW,w!("pleamar-layout-ci"),w!("Layout ñ 海"),WS_OVERLAPPEDWINDOW,
                area.x+24+i*30,area.y+24+i*30,320,200,None,None,Some(module.into()),None).unwrap();
            let _=ShowWindow(hwnd,SW_SHOWNOACTIVATE);
            SetWindowLongPtrW(hwnd,GWLP_USERDATA,1);
            send.send(hwnd.0 as isize).unwrap();
            let mut message=MSG::default();
            while GetMessageW(&mut message,None,0,0).0>0 {
                let _=TranslateMessage(&message);DispatchMessageW(&message);
            }
        });
        let hwnd=receive.recv_timeout(Duration::from_secs(5))?;
        apps.0.push((hwnd,thread));
        ids.push(inspect(HWND(hwnd as _)).ok_or("fixture missing")?.id);
    }
    let free:Vec<_>=ids.iter().map(|id|Ok((id.clone(),target(id)?.1.bounds))).collect::<Result<_>>()?;
    let foreground=unsafe { GetForegroundWindow() };
    let mut serial=Vec::new();let mut batched=Vec::new();
    for _ in 0..3 {
        let boxes=layout::arrange((&screen.work).into(),4,Layout::Grid,8)?;
        let moves:Vec<_>=ids.iter().cloned().zip(boxes.into_iter().map(Bounds::from)).collect();
        let started=Instant::now();
        for (id,bounds) in &moves { place(id,bounds)?; }
        serial.push(started.elapsed().as_micros() as u64);
        place_many(&free)?;
        let started=Instant::now();
        assert_eq!(place_many(&moves)?,4);
        batched.push(started.elapsed().as_micros() as u64);
        assert_eq!(place_many(&moves)?,0,"unchanged layouts must not resize applications");
        place_many(&free)?;
    }
    let rules=out.join("empty.conf");std::fs::write(&rules,"")?;
    let options=Options {monitors:BTreeSet::from([screen.name.clone()]),all:false,process:Some(std::process::id()),
        owner:None,state:out.join("recovery.json"),namespace:String::new(),seconds:None,rules,explicit_rules:true};
    let mut manager=Manager::open(&options)?;
    for layout in [Layout::Left,Layout::Right,Layout::Columns,Layout::Rows,Layout::Grid] {
        manager.set_mode(&screen.name,true,Some(layout))?;
        let mode=&manager.modes[&screen.name];
        let boxes=layout::arrange((&screen.work).into(),4,layout,(8.0*screen.scale).round() as i32)?;
        for (id,bounds) in mode.order.iter().zip(boxes) { assert_eq!(target(id)?.1.bounds,Bounds::from(bounds)); }
    }
    let order=manager.modes[&screen.name].order.clone();
    let destination=target(&order[2])?.1.bounds;
    manager.dropped(&order[0],POINT{x:destination.x+20,y:destination.y+20})?;
    manager.reconcile()?;
    assert_eq!(manager.modes[&screen.name].order[2],order[0]);
    assert_eq!(manager.modes[&screen.name].order[0],order[2]);
    assert_eq!(target(&order[0])?.1.bounds,destination);
    manager.set_mode(&screen.name,false,None)?;
    for (id,bounds) in &free { assert_eq!(target(id)?.1.bounds,*bounds); }
    assert_eq!(unsafe { GetForegroundWindow() },foreground,"layouts took focus");
    let serial_total:u64=serial.iter().sum();let batch_total:u64=batched.iter().sum();
    let report=json!({"serial_micros":serial,"batch_micros":batched,"ratio":batch_total as f64/serial_total as f64,
        "window_count":4,"fixture_delay_ms":40,"only_owned_windows":true,"physical_input":false,
        "checks":["five layouts read back","drop exchanges tiles","free positions restored","no focus change","unchanged geometry skipped"],
        "scope":"native placement acknowledgements; not compositor frame pacing or whole-product acceptance"});
    std::fs::write(out.join("report.json"),serde_json::to_vec_pretty(&report)?)?;
    assert!(batch_total*10<serial_total*8,"batched acknowledgement did not improve latency: {report}");
    drop(manager);drop(apps);
    unsafe { UnregisterClassW(class.lpszClassName,Some(module.into())) }?;
    Ok(())
}
