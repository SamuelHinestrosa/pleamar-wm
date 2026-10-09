"""Probe cross-process presentation on one owned secondary-display fixture.

This is not a global animation implementation or a compatibility certification.
No user window is changed; no input or foreground request is sent.
"""
from pathlib import Path
import argparse, ctypes as C, json, os, subprocess, sys, threading, time
from ctypes import wintypes as W

u=C.WinDLL('user32',use_last_error=True)
d=C.WinDLL('dwmapi')
for name,result,args in [
    ('CreateWindowExW',W.HWND,[W.DWORD,W.LPCWSTR,W.LPCWSTR,W.DWORD,C.c_int,C.c_int,C.c_int,C.c_int,W.HWND,W.HMENU,W.HINSTANCE,W.LPVOID]),
    ('ShowWindow',W.BOOL,[W.HWND,C.c_int]),('DestroyWindow',W.BOOL,[W.HWND]),
    ('GetForegroundWindow',W.HWND,[]),('SetProcessDpiAwarenessContext',W.BOOL,[W.HANDLE]),
    ('GetWindowLongPtrW',C.c_ssize_t,[W.HWND,C.c_int]),
    ('SetWindowLongPtrW',C.c_ssize_t,[W.HWND,C.c_int,C.c_ssize_t]),
    ('SetLayeredWindowAttributes',W.BOOL,[W.HWND,W.DWORD,W.BYTE,W.DWORD]),
    ('PeekMessageW',W.BOOL,[C.POINTER(W.MSG),W.HWND,W.UINT,W.UINT,W.UINT]),
    ('TranslateMessage',W.BOOL,[C.POINTER(W.MSG)]),('DispatchMessageW',C.c_ssize_t,[C.POINTER(W.MSG)]),
]:
    fn=getattr(u,name);fn.restype=result;fn.argtypes=args
d.DwmSetWindowAttribute.argtypes=[W.HWND,W.DWORD,C.c_void_p,W.DWORD];d.DwmSetWindowAttribute.restype=C.c_long
class MonitorInfo(C.Structure):
    _fields_=[('size',W.DWORD),('bounds',W.RECT),('work',W.RECT),('flags',W.DWORD)]
u.MonitorFromWindow.argtypes=[W.HWND,W.DWORD];u.MonitorFromWindow.restype=W.HANDLE
u.GetMonitorInfoW.argtypes=[W.HANDLE,C.POINTER(MonitorInfo)];u.GetMonitorInfoW.restype=W.BOOL

if '--fixture' in sys.argv:
    assert u.SetProcessDpiAwarenessContext(W.HANDLE(-4))
    x,y=map(int,sys.argv[-2:])
    hwnd=u.CreateWindowExW(0,'STATIC','Owned Marea transition probe',0x00cf0000,x,y,400,260,None,None,None,None)
    assert hwnd
    stop=threading.Event()
    threading.Thread(target=lambda:(sys.stdin.readline(),stop.set()),daemon=True).start()
    try:
        info=MonitorInfo(size=C.sizeof(MonitorInfo))
        assert u.GetMonitorInfoW(u.MonitorFromWindow(hwnd,0),C.byref(info)) and not info.flags&1
        assert info.work.left<=x and info.work.top<=y and x+400<=info.work.right and y+260<=info.work.bottom
        u.ShowWindow(hwnd,4)
        print(json.dumps(dict(hwnd=hwnd,pid=os.getpid())),flush=True)
        message=W.MSG()
        while not stop.wait(.005):
            while u.PeekMessageW(C.byref(message),None,0,0,1):
                u.TranslateMessage(C.byref(message));u.DispatchMessageW(C.byref(message))
    finally:u.DestroyWindow(hwnd)
    raise SystemExit(0)

parser=argparse.ArgumentParser(description=__doc__)
parser.add_argument('--binary',type=Path,required=True)
parser.add_argument('--monitor',required=True)
parser.add_argument('--output',type=Path,required=True)
args=parser.parse_args()
from PIL import Image
assert u.SetProcessDpiAwarenessContext(W.HANDLE(-4))
binary=args.binary.resolve(strict=True)
flags=subprocess.CREATE_NO_WINDOW|subprocess.BELOW_NORMAL_PRIORITY_CLASS
screens=json.loads(subprocess.check_output([str(binary),'monitors'],creationflags=flags,text=True))
monitor=next(m for m in screens if m['name']==args.monitor and not m['primary'])
assert monitor['work']['width']>=500 and monitor['work']['height']>=360
out=args.output.resolve();out.mkdir(parents=True,exist_ok=False)
env=dict(os.environ,PLEAMAR_CONFIG=str(out/'config'),PLEAMAR_WM_NAMESPACE='transition-probe-'+str(os.getpid()))
foreground=u.GetForegroundWindow()
report=dict(monitor=monitor['name'],user_windows_changed=False,input_injected=False,global_animations=False)
child=subprocess.Popen([sys.executable,__file__,'--fixture',str(monitor['work']['x']+50),str(monitor['work']['y']+50)],
    stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True,creationflags=flags)
hwnd=None;original=None
try:
    owner=json.loads(child.stdout.readline());assert owner['pid']==child.pid
    hwnd=owner['hwnd'];original=u.GetWindowLongPtrW(hwnd,-20);assert not original&0x80000
    def picture(label):
        assert u.GetForegroundWindow()==foreground,'Foreground changed during the probe'
        path=out/(label+'.png')
        start=time.perf_counter()
        run=subprocess.run([str(binary),'agent','look',str(child.pid),str(path)],env=env,
            capture_output=True,text=True,encoding='utf-8',errors='replace',timeout=20,creationflags=flags)
        result=dict(exit=run.returncode,elapsed_ms=(time.perf_counter()-start)*1000)
        if run.returncode:result['error']=(run.stdout+run.stderr).strip()
        else:
            with Image.open(path) as image:
                image=image.convert('RGBA')
                result.update(size=list(image.size),center=list(image.getpixel((image.width//2,image.height//2))))
        report[label]=result
    time.sleep(.1)
    picture('normal')
    flag=W.BOOL(True)
    report['foreign_cloak']=hex(d.DwmSetWindowAttribute(hwnd,13,C.byref(flag),C.sizeof(flag))&0xffffffff)
    flag=W.BOOL(False);d.DwmSetWindowAttribute(hwnd,13,C.byref(flag),C.sizeof(flag))
    C.set_last_error(0);u.SetWindowLongPtrW(hwnd,-20,original|0x80000)
    assert C.get_last_error()==0
    assert u.SetLayeredWindowAttributes(hwnd,0,0,2)
    time.sleep(.1)
    picture('alpha_zero')
finally:
    if hwnd and original is not None:
        u.SetLayeredWindowAttributes(hwnd,0,255,2)
        u.SetWindowLongPtrW(hwnd,-20,original)
        report['style_restored']=u.GetWindowLongPtrW(hwnd,-20)==original
    child.communicate('\n',timeout=5)
    report['foreground_unchanged']=u.GetForegroundWindow()==foreground
    (out/'report.json').write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8')
print(json.dumps(report,indent=2))
assert report['style_restored'] and report['foreground_unchanged']
