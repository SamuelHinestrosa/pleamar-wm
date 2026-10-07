use super::*;
use std::{path::PathBuf, process::{Child, Command}, os::windows::process::CommandExt};

struct Files(PathBuf);
impl Files {
    fn new() -> Self {
        let stamp=std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path=std::env::temp_dir().join(format!("pleamar launch ñ ' {} {stamp}",std::process::id()));
        std::fs::create_dir(&path).unwrap();Self(path)
    }
    fn file(&self,name:&str) -> PathBuf { self.0.join(name) }
}
impl Drop for Files {
    fn drop(&mut self) {
        for name in ["result.txt","pid.txt"] { let _=std::fs::remove_file(self.file(name)); }
        let _=std::fs::remove_dir(&self.0);
    }
}
struct ChildGuard(Child);
impl Drop for ChildGuard { fn drop(&mut self) { let _=self.0.kill();let _=self.0.wait(); } }
fn quoted(value:&str) -> String { format!("'{}'",value.replace('\'',"''")) }
fn write(path:&Path,text:&str) -> String {
    format!("[IO.File]::WriteAllText({}, {}, (New-Object Text.UTF8Encoding $false))",quoted(&path.to_string_lossy()),text)
}
fn wait_file(path:&Path) -> String {
    let until=Instant::now()+Duration::from_secs(20);
    loop {
        if let Ok(text)=std::fs::read_to_string(path) { if !text.is_empty() { return text; } }
        assert!(Instant::now()<until,"native launcher did not write {}",path.display());
        std::thread::sleep(Duration::from_millis(10));
    }
}
fn leaf_command(path:&Path) -> String {
    format!("$p = Start-Process -FilePath {} -ArgumentList @('--ignored', '--exact', 'windows_backend::launch::tests::launch_leaf') -WindowStyle Hidden -PassThru; {}",
        quoted(&std::env::current_exe().unwrap().to_string_lossy()),write(path,"$p.Id.ToString()"))
}
fn process(pid:u32) -> OwnedHandle {
    unsafe { own(OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION|PROCESS_SYNCHRONIZE,false,pid).unwrap()) }
}
fn wait_empty(commands:&mut Launches) {
    // A signalled process handle does not require job accounting to have
    // reached zero yet. Exercise the production deadline, not a future Instant
    // followed by an immediate assertion about asynchronous Windows teardown.
    let deadline=Instant::now()+Duration::from_secs(5);
    loop {
        let now=Instant::now();
        commands.poll(now).unwrap();
        if commands.running.is_empty() {
            assert!(commands.wait(now).is_none());
            return;
        }
        assert!(now<deadline,"terminated launch groups did not retire: {:?}",
            commands.running.iter().map(|item|(item.pid,item.active(),item.process.is_none())).collect::<Vec<_>>());
        std::thread::sleep(commands.wait(now).unwrap().min(Duration::from_millis(25)));
    }
}

#[test]
fn native_arguments_round_trip_and_reject_invalid_limits() {
    use windows::Win32::UI::Shell::CommandLineToArgvW;
    let path="C:\\A 'ñ'\\player.exe";
    let args:Vec<String>=["", "C:\\a folder\\", "C:\\it's 海\\$HOME; $(exit 9).txt", "quoted\"value", "slash\\\"quote", "tabs\tand\nlines"].into_iter().map(str::to_owned).collect();
    let command=executable_line(path,&args).unwrap();let mut count=0;
    let argv=unsafe { CommandLineToArgvW(PCWSTR(command.as_ptr()),&mut count) };
    assert!(!argv.is_null());
    let actual:Vec<String>=unsafe { std::slice::from_raw_parts(argv,count as usize) }.iter().map(|s|unsafe { s.to_string().unwrap() }).collect();
    unsafe { let _=LocalFree(Some(HLOCAL(argv.cast()))); }
    assert_eq!(actual,[vec![path.to_owned()],args].concat());
    for bad in ["player.exe","C:\\app.ps1","C:\\bad\0.exe","C:\\bad\".exe"] { assert!(executable_line(bad,&[]).is_err()); }
    assert!(executable_line(path,&["nul\0arg".into()]).is_err());
    assert!(executable_line(path,&["🚀".repeat(17000)]).is_err());
}

#[test]
fn native_launch_reports_creation_errors_and_owns_its_process() {
    let files=Files::new();let mut commands=Launches::default();
    let missing=files.file("missing.exe");
    assert!(commands.executable(&missing.to_string_lossy(),&[]).is_err());
    assert!(commands.running.is_empty(),"a failed creation must not retain a process group");
    let executable=std::env::current_exe().unwrap();
    let args=["--ignored","--exact","windows_backend::launch::tests::launch_leaf"].map(str::to_owned);
    let child=process(commands.executable(&executable.to_string_lossy(),&args).unwrap());
    assert_eq!(unsafe { WaitForSingleObject(handle(&child),0) },WAIT_TIMEOUT);
    drop(commands);
    assert_eq!(unsafe { WaitForSingleObject(handle(&child),5000) },WAIT_OBJECT_0);
}

#[test]
fn scene_commands_preserve_unicode_quotes_and_validate_native_limits() {
    for bad in ["", "   ", "exit\0 0", &"x".repeat(16001), &"x".repeat(15000)] {
        assert!(command_line(bad).is_err());
    }
    let files=Files::new();let mut commands=Launches::default();
    let expected="Escena ñ 🚀: 'comillas', \"espacios\" y 海\nsegunda línea";
    let command=write(&files.file("result.txt"),&quoted(expected));
    let pid=commands.start(&command).unwrap();
    let running=process(pid);
    assert_eq!(unsafe { WaitForSingleObject(handle(&running),20_000) },WAIT_OBJECT_0);
    assert_eq!(wait_file(&files.file("result.txt")),expected);
    drop(running);
    wait_empty(&mut commands);
    let failed=process(commands.start("exit 23").unwrap());
    assert_eq!(unsafe { WaitForSingleObject(handle(&failed),20_000) },WAIT_OBJECT_0);
    assert_eq!(commands.running[0].exit().unwrap(),Some(23));
    assert!(commands.running[0].process.is_none(),"exit must release the shell handle while retaining the job");
    assert_eq!(commands.running[0].exit().unwrap(),None);
    drop(failed);
    wait_empty(&mut commands);
}

#[test]
fn scene_launch_retains_descendants_and_closes_only_its_own_group() {
    let mut sibling=ChildGuard(Command::new(std::env::current_exe().unwrap())
        .args(["--ignored","--exact","windows_backend::launch::tests::launch_leaf"])
        .creation_flags(CREATE_NO_WINDOW.0).spawn().unwrap());
    let files=Files::new();let mut commands=Launches::default();
    let shell=commands.start(&leaf_command(&files.file("pid.txt"))).unwrap();
    let parent=process(shell);
    let child=process(wait_file(&files.file("pid.txt")).trim().parse().unwrap());
    assert_eq!(unsafe { WaitForSingleObject(handle(&parent),20_000) },WAIT_OBJECT_0);
    commands.poll(Instant::now()+Duration::from_secs(1)).unwrap();
    assert_eq!(commands.running.len(),1,"the child's job must survive its shell");
    assert!(commands.running[0].process.is_none(),"only the job owns the surviving descendant");
    assert_eq!(unsafe { WaitForSingleObject(handle(&child),0) },WAIT_TIMEOUT);
    drop(commands);
    assert_eq!(unsafe { WaitForSingleObject(handle(&child),5000) },WAIT_OBJECT_0);
    assert!(sibling.0.try_wait().unwrap().is_none(),"unrelated processes must survive");
}

#[test]
fn scene_launch_descendants_end_after_forced_owner_exit() {
    let files=Files::new();
    let mut owner=ChildGuard(Command::new(std::env::current_exe().unwrap())
        .args(["--ignored","--exact","windows_backend::launch::tests::launch_owner"])
        .env("PLEAMAR_LAUNCH_TEST_DIR",&files.0).creation_flags(CREATE_NO_WINDOW.0).spawn().unwrap());
    let child=process(wait_file(&files.file("pid.txt")).trim().parse().unwrap());
    assert_eq!(unsafe { WaitForSingleObject(handle(&child),0) },WAIT_TIMEOUT);
    owner.0.kill().unwrap();owner.0.wait().unwrap();
    assert_eq!(unsafe { WaitForSingleObject(handle(&child),5000) },WAIT_OBJECT_0);
}

#[test]
#[ignore = "owned subprocess helper, no windows or input"]
fn launch_leaf() { std::thread::sleep(Duration::from_secs(40)); }

#[test]
#[ignore = "owned subprocess helper, requires its isolated test directory"]
fn launch_owner() {
    let directory=PathBuf::from(std::env::var_os("PLEAMAR_LAUNCH_TEST_DIR").expect("isolated test directory"));
    assert!(directory.is_absolute() && directory.is_dir());
    // Production already belongs to the engine's job. Exercise that nesting
    // in this disposable owner process, without assigning the test harness.
    let outer=unsafe { own(CreateJobObjectW(None,PCWSTR::null()).unwrap()) };
    let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE|JOB_OBJECT_LIMIT_BREAKAWAY_OK;
    unsafe {
        SetInformationJobObject(handle(&outer),JobObjectExtendedLimitInformation,&limits as *const _ as _,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32).unwrap();
        AssignProcessToJobObject(handle(&outer),GetCurrentProcess()).unwrap();
    }
    let mut commands=Launches::default();
    commands.start(&leaf_command(&directory.join("pid.txt"))).unwrap();
    std::thread::sleep(Duration::from_secs(40));
}
