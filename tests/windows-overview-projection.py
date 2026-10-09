"""Project an owned secondary-display window onto a closed output elsewhere.

No visible surface is opened on the destination and no input is injected.
"""
from pathlib import Path
import argparse,ctypes as C,json,os,subprocess,sys,time
from ctypes import wintypes as W

p=argparse.ArgumentParser(description=__doc__)
p.add_argument('--binary',type=Path,required=True)
p.add_argument('--source',required=True)
p.add_argument('--destination',required=True)
p.add_argument('--output',type=Path,required=True)
a=p.parse_args();binary=a.binary.resolve(strict=True)
flags=subprocess.CREATE_NO_WINDOW|subprocess.BELOW_NORMAL_PRIORITY_CLASS
screens=json.loads(subprocess.check_output([str(binary),'monitors'],text=True,creationflags=flags))
source=next(m for m in screens if m['name']==a.source and not m['primary'])
destination=next(m for m in screens if m['name']==a.destination and m['name']!=a.source)
out=a.output.resolve();out.mkdir(parents=True,exist_ok=False)
u=C.WinDLL('user32');u.GetForegroundWindow.restype=W.HWND
u.IsWindowVisible.argtypes=[W.HWND];u.IsWindowVisible.restype=W.BOOL
u.GetWindowThreadProcessId.argtypes=[W.HWND,C.POINTER(W.DWORD)];u.GetWindowThreadProcessId.restype=W.DWORD
u.GetWindowRect.argtypes=[W.HWND,C.POINTER(W.RECT)];u.GetWindowRect.restype=W.BOOL
u.GetWindowLongPtrW.argtypes=[W.HWND,C.c_int];u.GetWindowLongPtrW.restype=C.c_ssize_t
callback=C.WINFUNCTYPE(W.BOOL,W.HWND,W.LPARAM)
u.EnumWindows.argtypes=[callback,W.LPARAM];u.EnumWindows.restype=W.BOOL
foreground=u.GetForegroundWindow();renderer=None;fixture=None
report=dict(passed=False,source=source['name'],destination=destination['name'],input_injected=False)
env=dict(os.environ,PLEAMAR_CONFIG=str(out/'config'),PLEAMAR_SOCKET_DIR='projection-'+str(os.getpid()))
scene=out/'projection.plm'
scene.write_text('''scene Projection {
    surface { size: 2, 2; open: false; keyboard: none }
    windows win max 4
    repeat i in 0..4 {
        fact win.$i.native.x = 0
        fact win.$i.native.y = 0
        fact win.$i.native.width = 0
        fact win.$i.native.height = 0
    }
}''',encoding='utf-8')
def ask(line):
    r=subprocess.run([str(binary),'--say','projection',line],env=env,creationflags=flags,
        text=True,capture_output=True,timeout=4)
    if r.returncode:raise RuntimeError(r.stdout+r.stderr)
    return r.stdout.strip()
try:
    fixture=subprocess.Popen([sys.executable,str(Path(__file__).with_name('windows-transition-probe.py')),'--fixture',
        str(source['work']['x']+50),str(source['work']['y']+50)],creationflags=flags,
        stdin=subprocess.PIPE,stdout=subprocess.PIPE,stderr=subprocess.PIPE,text=True)
    owner=json.loads(fixture.stdout.readline());assert owner['pid']==fixture.pid
    with (out/'scene.log').open('w',encoding='utf-8') as log:
        renderer=subprocess.Popen([str(binary),'--scene',str(scene),'--screen',destination['name'],
            '--preview-monitor','all','--preview-project','--preview-process',str(fixture.pid),
            '--no-hud','--stall','0'],env=env,creationflags=flags,stdout=log,stderr=log)
        end=time.monotonic()+20
        while True:
            assert renderer.poll() is None,'Hidden renderer exited'
            assert u.GetForegroundWindow()==foreground,'Foreground changed'
            try:
                if float(ask('get win.0.native.width'))>0:break
            except (RuntimeError,ValueError,subprocess.TimeoutExpired):pass
            assert time.monotonic()<end,'Projection was never published'
            time.sleep(.02)
        catalog=json.loads(subprocess.check_output([str(binary),'windows'],creationflags=flags,text=True))
        window=next(w for w in catalog if w['process']==fixture.pid)
        bounds=window['bounds'];origin=destination['bounds'];scale=destination['scale']
        expected=[(bounds['x']-origin['x'])/scale,(bounds['y']-origin['y'])/scale,bounds['width']/scale,bounds['height']/scale]
        actual=[float(ask('get win.0.native.'+key)) for key in ['x','y','width','height']]
        assert all(abs(left-right)<.01 for left,right in zip(expected,actual))
        assert ask('get win.count')=='1' and float(ask('get win.0.width'))==0
        visible=[]
        @callback
        def inspect(hwnd,_):
            pid=W.DWORD();u.GetWindowThreadProcessId(hwnd,C.byref(pid))
            if pid.value==renderer.pid and u.IsWindowVisible(hwnd):visible.append(hwnd)
            return True
        assert u.EnumWindows(inspect,0)
        # The native renderer keeps its transparent composition HWND shown even
        # for a closed surface. It is not an interactive destination window.
        for hwnd in visible:
            rect=W.RECT();assert u.GetWindowRect(hwnd,C.byref(rect))
            assert rect.right-rect.left<=3 and rect.bottom-rect.top<=3
            assert u.GetWindowLongPtrW(hwnd,-20)&0x08000000, 'Destination can activate'
        report.update(passed=True,expected=expected,actual=actual,no_capture=True,destination_closed=True)
finally:
    if renderer and renderer.poll() is None:
        try:ask('quit');renderer.wait(timeout=5)
        except Exception:renderer.kill();renderer.wait(timeout=5)
    if fixture:fixture.communicate('\n',timeout=5)
    report['foreground_unchanged']=u.GetForegroundWindow()==foreground
    (out/'report.json').write_text(json.dumps(report,indent=2)+'\n',encoding='utf-8')
print(json.dumps(report,indent=2))
assert report['passed'] and report['foreground_unchanged']
