//! OS package activation on an explicitly opted-in disposable runner only.
use super::*;

struct Owned(Vec<(HWND,u32)>);
impl Drop for Owned {
    fn drop(&mut self) {
        for &(hwnd,pid) in &self.0 {
            let mut current=0;unsafe {GetWindowThreadProcessId(hwnd,Some(&mut current));}
            if current==pid {unsafe {let _=PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0));}}
        }
    }
}
fn await_value<T>(what:&str,mut read:impl FnMut()->Result<Option<T>>) -> Result<T> {
    let deadline=Instant::now()+Duration::from_secs(20);
    loop {
        if let Some(value)=read()? {return Ok(value);}
        if Instant::now()>=deadline {return Err(format!("timed out waiting for {what}").into());}
        std::thread::sleep(Duration::from_millis(25));
    }
}
fn arrival(root:&Path,seen:&mut HashSet<u32>,owned:&mut Owned) -> Result<Value> {
    await_value("packaged fixture window",|| {
        for entry in std::fs::read_dir(root)? {
            let path=entry?.path();
            if !path.file_name().unwrap_or_default().to_string_lossy().starts_with("launch-") {continue;}
            let value:Value=match serde_json::from_slice(&std::fs::read(&path)?) {Ok(v)=>v,Err(e) if e.is_eof()=>continue,Err(e)=>return Err(e.into())};
            let pid=value["pid"].as_u64().ok_or("fixture PID missing")? as u32;
            if seen.contains(&pid) {continue;}
            let hwnd=HWND(value["hwnd"].as_u64().ok_or("fixture HWND missing")? as usize as _);
            let Some(window)=inspect(hwnd) else {continue;};
            if window.process!=pid {return Err("fixture HWND was reused".into());}
            seen.insert(pid);owned.0.push((hwnd,pid));return Ok(Some(value));
        }
        Ok(None)
    })
}
fn close(value:&Value) -> Result<()> {
    let hwnd=HWND(value["hwnd"].as_u64().ok_or("fixture HWND missing")? as usize as _);
    let pid=value["pid"].as_u64().ok_or("fixture PID missing")? as u32;
    let mut current=0;unsafe {GetWindowThreadProcessId(hwnd,Some(&mut current));}
    if current!=pid {return Err("refusing to close a different process".into());}
    unsafe {PostMessageW(Some(hwnd),WM_CLOSE,WPARAM(0),LPARAM(0))}?;
    await_value("owned packaged window close",||Ok((!unsafe {IsWindow(Some(hwnd))}.as_bool()).then_some(())))
}

#[test]
#[ignore = "registers/activates only the owned test package on a disposable GitHub-hosted Windows desktop"]
fn native_packaged_dock_activation() -> Result<()> {
    for (name,expected) in [("GITHUB_ACTIONS","true"),("RUNNER_ENVIRONMENT","github-hosted"),("PLEAMAR_WM_CI_DOCK_PACKAGE","1")] {
        if std::env::var(name).as_deref()!=Ok(expected) {return Err("packaged dock acceptance requires its explicit disposable CI step".into());}
    }
    let id=std::env::var("PLEAMAR_WM_PACKAGE_AUMID")?;
    if !id.starts_with("Pleamar.NativeDockTest_") || !id.ends_with("!Fixture") {return Err("refusing another package identity".into());}
    let root=PathBuf::from(std::env::var("PLEAMAR_WM_PACKAGE_OUTPUT")?);
    let temporary=PathBuf::from(std::env::var("RUNNER_TEMP")?).canonicalize()?;
    let root=root.canonicalize()?;
    if root==temporary || !root.starts_with(&temporary) {return Err("package evidence must be below RUNNER_TEMP".into());}
    let mut owned=Owned(Vec::new());let mut seen=HashSet::new();
    let pin=Program {target:Target::Application(id.clone()),name:"Owned packaged dock application".into()};
    let path=root.join("pins.json");let mut dock=Dock::new(path.clone())?;dock.remember(pin.clone());
    let mut launches=launch::Launches::default();let mut events=Vec::new();
    let result=(|| -> Result<()> {
        dock.open(&pin.key(),&[],&mut launches)?;
        let first=arrival(&root,&mut seen,&mut owned)?;
        assert!(first["package"].as_str().is_some_and(|p|p.starts_with("Pleamar.NativeDockTest_")));
        let hwnd=HWND(first["hwnd"].as_u64().unwrap() as usize as _);
        let window=inspect(hwnd).ok_or("packaged window disappeared")?;
        let resolved=program(&window)?;
        assert_eq!(resolved.target,pin.target,"packaged metadata must keep its activation identity");
        assert!(!resolved.name.is_empty());events.push(json!({"stage":"activate-and-resolve","window":first,"key":resolved.key(),"name":resolved.name}));
        dock.remember(resolved);dock.pin(&pin.key(),true)?;
        drop(launches);
        assert!(unsafe {IsWindow(Some(hwnd))}.as_bool(),"OS-owned activation must survive the scene launch collection");
        close(&first)?;
        let reloaded=Dock::new(path)?;assert_eq!(reloaded.pins.len(),1);
        launches=launch::Launches::default();reloaded.open(&pin.key(),&[],&mut launches)?;
        let second=arrival(&root,&mut seen,&mut owned)?;events.push(json!({"stage":"restart-pinned-application","window":second}));close(&second)?;
        let file=root.join("owned ñ 海 ' $HOME.plmdock");std::fs::write(&file,"owned package activation file")?;
        reloaded.open(&pin.key(),&[file.to_string_lossy().into_owned()],&mut launches)?;
        let third=arrival(&root,&mut seen,&mut owned)?;
        let args=third["args"].as_array().ok_or("package argument report missing")?;
        assert!(args.iter().any(|arg|arg.as_str().is_some_and(|s|std::fs::canonicalize(s).ok().as_ref()==Some(&file))),
            "file activation did not identify the requested Unicode file: {args:?}");
        events.push(json!({"stage":"native-file-activation","window":third}));close(&third)?;
        Ok(())
    })();
    let report=json!({"passed":result.is_ok(),"aumid":id,"events":events,"physical_input":false,"actual_os_file_drag":false,
        "store_download_tested":false,"error":result.as_ref().err().map(|e|e.to_string())});
    std::fs::write(root.join("activation-report.json"),serde_json::to_vec_pretty(&report)?)?;
    result
}
