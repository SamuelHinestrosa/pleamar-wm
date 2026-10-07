//! Authored scene commands belong to this preview's lifetime, including their children.
use super::*;
use base64::Engine;
use std::os::windows::{ffi::OsStrExt, io::{AsRawHandle, FromRawHandle, OwnedHandle}};
use windows::Win32::System::JobObjects::*;

fn handle(owned:&OwnedHandle) -> HANDLE { HANDLE(owned.as_raw_handle()) }
unsafe fn own(raw:HANDLE) -> OwnedHandle { unsafe { OwnedHandle::from_raw_handle(raw.0) } }

struct Attributes { _storage:Vec<usize>, list:LPPROC_THREAD_ATTRIBUTE_LIST }
impl Attributes {
    fn job(job:&HANDLE) -> Result<Self> { unsafe {
        let mut size=0;
        let _=InitializeProcThreadAttributeList(None,1,None,&mut size);
        if size==0 { return Err("Windows did not provide process attribute storage".into()); }
        let mut storage=vec![0usize;size.div_ceil(size_of::<usize>())];
        let list=LPPROC_THREAD_ATTRIBUTE_LIST(storage.as_mut_ptr().cast());
        InitializeProcThreadAttributeList(Some(list),1,None,&mut size)?;
        let attributes=Self {_storage:storage,list};
        UpdateProcThreadAttribute(list,0,PROC_THREAD_ATTRIBUTE_JOB_LIST as usize,
            Some(job as *const _ as _),size_of::<HANDLE>(),None,None)?;
        Ok(attributes)
    } }
}
impl Drop for Attributes { fn drop(&mut self) { unsafe { DeleteProcThreadAttributeList(self.list); } } }

struct Launch { job:OwnedHandle, process:Option<OwnedHandle>, pid:u32 }
impl Launch {
    fn active(&self) -> Result<bool> {
        let mut accounting=JOBOBJECT_BASIC_ACCOUNTING_INFORMATION::default();
        unsafe { QueryInformationJobObject(Some(handle(&self.job)),JobObjectBasicAccountingInformation,
            &mut accounting as *mut _ as _,size_of::<JOBOBJECT_BASIC_ACCOUNTING_INFORMATION>() as u32,None) }?;
        Ok(accounting.ActiveProcesses!=0)
    }
    fn exit(&mut self) -> Result<Option<u32>> {
        let Some(process)=&self.process else { return Ok(None); };
        if unsafe { WaitForSingleObject(handle(process),0) }!=WAIT_OBJECT_0 { return Ok(None); }
        let mut code=0;
        unsafe { GetExitCodeProcess(handle(process),&mut code) }?;
        // Job accounting can lag process signalling. The exit code is consumed
        // once; release our process reference while waiting for the whole job.
        self.process=None;
        Ok(Some(code))
    }
}

pub(super) struct Launches { running:Vec<Launch>, next:Instant }
impl Default for Launches {
    fn default() -> Self { Self { running:Vec::new(), next:Instant::now() } }
}
impl Launches {
    pub(super) fn start(&mut self,line:&str) -> Result<u32> {
        let mut command=command_line(line)?;
        self.poll(Instant::now())?;
        if self.running.len()>=16 { return Err("at most 16 scene-launched process groups may run at once".into()); }
        let root=std::env::var_os("SystemRoot").ok_or("SystemRoot is unavailable")?;
        let executable=std::path::PathBuf::from(root).join("System32/WindowsPowerShell/v1.0/powershell.exe");
        let executable:Vec<u16>=executable.as_os_str().encode_wide().chain([0]).collect();
        let job=unsafe { own(CreateJobObjectW(None,PCWSTR::null())?) };
        let job_handle=handle(&job);
        let mut limits=JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
        limits.BasicLimitInformation.LimitFlags=JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        unsafe { SetInformationJobObject(job_handle,JobObjectExtendedLimitInformation,
            &limits as *const _ as _,size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32) }?;
        // Assign at creation, not after spawning: even termination between those
        // operations must not leave a suspended or running orphan behind.
        let attributes=Attributes::job(&job_handle)?;
        let mut startup=STARTUPINFOEXW::default();
        startup.StartupInfo.cb=size_of::<STARTUPINFOEXW>() as u32;
        startup.lpAttributeList=attributes.list;
        let mut info=PROCESS_INFORMATION::default();
        unsafe { CreateProcessW(PCWSTR(executable.as_ptr()),Some(PWSTR(command.as_mut_ptr())),None,None,false,
            CREATE_NO_WINDOW|EXTENDED_STARTUPINFO_PRESENT,None,PCWSTR::null(),&startup.StartupInfo,&mut info) }?;
        let process=unsafe { own(info.hProcess) };
        let _thread=unsafe { own(info.hThread) };
        self.running.push(Launch {job,process:Some(process),pid:info.dwProcessId});
        self.next=Instant::now()+Duration::from_millis(250);
        Ok(info.dwProcessId)
    }
    pub(super) fn poll(&mut self,now:Instant) -> Result<()> {
        if self.running.is_empty() || now<self.next { return Ok(()); }
        self.next=now+Duration::from_millis(250);
        let mut index=0;
        while index<self.running.len() {
            let launch=&mut self.running[index];
            if let Some(code)=launch.exit()? {
                if code!=0 { eprintln!("windows launch: process {} exited with code {code}",launch.pid); }
            }
            // A shell can exit before its GUI child. Keep that child's job,
            // without retaining completed groups for the rest of the session.
            if launch.active()? { index+=1; } else { self.running.swap_remove(index); }
        }
        Ok(())
    }
    pub(super) fn wait(&self,now:Instant) -> Option<Duration> {
        (!self.running.is_empty()).then(||self.next.saturating_duration_since(now))
    }
}

fn command_line(line:&str) -> Result<Vec<u16>> {
    if line.trim().is_empty() || line.contains('\0') || line.len()>16000 { return Err("invalid or oversized scene launch command".into()); }
    // PowerShell's UTF-16 encoded form preserves nested quotes, newlines and
    // Unicode without adding a second command-line parser's escaping rules.
    let bytes:Vec<u8>=line.encode_utf16().flat_map(u16::to_le_bytes).collect();
    let encoded=base64::engine::general_purpose::STANDARD.encode(bytes);
    let command=format!("powershell.exe -NoLogo -NoProfile -NonInteractive -EncodedCommand {encoded}");
    let command:Vec<u16>=command.encode_utf16().chain([0]).collect();
    if command.len()>32767 { return Err("scene launch command exceeds the Windows command-line limit".into()); }
    Ok(command)
}

#[cfg(test)]
#[path = "windows_launch_tests.rs"]
mod tests;
