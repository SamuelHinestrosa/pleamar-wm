from pathlib import Path
import ctypes as C
from ctypes import wintypes as W
import hashlib, json, os, subprocess, time
from PIL import Image

import argparse

parser = argparse.ArgumentParser(description="Exercise the native dock on an explicit non-primary monitor, without injecting input.")
parser.add_argument('--binary', type=Path, required=True)
parser.add_argument('--fixture', type=Path, required=True)
parser.add_argument('--monitor', required=True)
parser.add_argument('--output', type=Path, required=True)
options = parser.parse_args()
repo = Path(__file__).resolve().parents[1]
binary = options.binary.resolve(strict=True)
fixture = options.fixture.resolve(strict=True)
output = options.output.resolve()
assert not output.exists(), 'Use a fresh output directory; preserve previous evidence'
assert binary.is_file() and fixture.is_file()
flags = subprocess.CREATE_NO_WINDOW | subprocess.BELOW_NORMAL_PRIORITY_CLASS
screens = json.loads(subprocess.check_output([str(binary), 'monitors'], creationflags=flags, encoding='utf-8'))
matching = [s for s in screens if s['name'] == options.monitor and not s['primary']]
assert len(matching) == 1, 'The explicit non-primary monitor is unavailable'
screen = matching[0]
output.mkdir()
env = os.environ.copy()
env.update(PLEAMAR_CONFIG=str(output / 'config'), APPDATA=str(output / 'appdata'), LOCALAPPDATA=str(output / 'localappdata'),
           PLEAMAR_SOCKET_DIR=f'native-dock-acceptance-{os.getpid()}', PLEAMAR_NO_RELAUNCH='1',
           PLEAMAR_TEST_WINDOWS='1', PLEAMAR_DOCK_FIXTURE_MONITOR=screen['name'], PLEAMAR_DOCK_FIXTURE_ROOT=str(output))
source = (repo / 'examples/windows-dock.plm').read_text(encoding='utf-8')
surface = 'surface { size: 900, 420; kind: window; title: "pleamar · native dock" }'
assert source.count(surface) == 1
source = source.replace(surface, 'surface { size: 900, 420; keyboard: none; anchor: center; reserve: 0; rate: 30 }')
scene_file = output / 'native-dock.plm'
scene_file.write_text(source, encoding='utf-8')
subprocess.run([str(binary), '--check', str(scene_file)], check=True, env=env, creationflags=flags, capture_output=True)

user = C.WinDLL('user32', use_last_error=True)
gdi = C.WinDLL('gdi32', use_last_error=True)
callback = C.WINFUNCTYPE(W.BOOL, W.HWND, W.LPARAM)
for lib, name, result, args in [
    (user,'EnumWindows',W.BOOL,[callback,W.LPARAM]), (user,'GetWindowThreadProcessId',W.DWORD,[W.HWND,C.POINTER(W.DWORD)]),
    (user,'GetWindowTextW',C.c_int,[W.HWND,W.LPWSTR,C.c_int]), (user,'IsWindowVisible',W.BOOL,[W.HWND]), (user,'GetWindowRect',W.BOOL,[W.HWND,C.POINTER(W.RECT)]),
    (user,'GetForegroundWindow',W.HWND,[]), (user,'SetProcessDpiAwarenessContext',W.BOOL,[W.HANDLE]),
    (user,'PostMessageW',W.BOOL,[W.HWND,W.UINT,W.WPARAM,W.LPARAM]), (user,'GetDC',W.HDC,[W.HWND]),
    (user,'ReleaseDC',C.c_int,[W.HWND,W.HDC]), (gdi,'CreateCompatibleDC',W.HDC,[W.HDC]),
    (gdi,'CreateCompatibleBitmap',W.HBITMAP,[W.HDC,C.c_int,C.c_int]), (gdi,'SelectObject',W.HANDLE,[W.HDC,W.HANDLE]),
    (gdi,'BitBlt',W.BOOL,[W.HDC,C.c_int,C.c_int,C.c_int,C.c_int,W.HDC,C.c_int,C.c_int,W.DWORD]),
    (gdi,'DeleteDC',W.BOOL,[W.HDC]), (gdi,'DeleteObject',W.BOOL,[W.HANDLE]),
    (gdi,'GetDIBits',C.c_int,[W.HDC,W.HBITMAP,W.UINT,W.UINT,C.c_void_p,C.c_void_p,W.UINT])]:
    fn = getattr(lib,name); fn.restype=result; fn.argtypes=args
assert user.SetProcessDpiAwarenessContext(W.HANDLE(-4))
class Header(C.Structure):
    _fields_=[('size',W.DWORD),('width',W.LONG),('height',W.LONG),('planes',W.WORD),('bits',W.WORD),
              ('compression',W.DWORD),('image_size',W.DWORD),('xppm',W.LONG),('yppm',W.LONG),('used',W.DWORD),('important',W.DWORD)]
class Info(C.Structure):
    _fields_=[('header',Header),('colors',W.DWORD*3)]
owned_pids=set()
images=[]
commands=[]
scene=None
initial=None
report=dict(passed=False,monitor=screen,physical_input=False,installed_product_changed=False,stages=[],images=images,
            binaries={str(p):hashlib.sha256(p.read_bytes()).hexdigest() for p in [binary,fixture]})

def windows(pid):
    found=[]
    @callback
    def visit(hwnd,_):
        process=W.DWORD();user.GetWindowThreadProcessId(hwnd,C.byref(process))
        if process.value==pid and user.IsWindowVisible(hwnd): found.append(hwnd)
        return True
    assert user.EnumWindows(visit,0)
    return found

def canvases(pid):
    result=[]
    for hwnd in windows(pid):
        title=C.create_unicode_buffer(512);user.GetWindowTextW(hwnd,title,len(title))
        if not title.value.startswith('pleamar input'):result.append(hwnd)
    return result

def guard():
    foreground=user.GetForegroundWindow();pid=W.DWORD();user.GetWindowThreadProcessId(foreground,C.byref(pid))
    assert pid.value not in owned_pids, 'An owned test window unexpectedly took foreground'
    for process in owned_pids:
        for hwnd in windows(process):
            box=W.RECT();assert user.GetWindowRect(hwnd,C.byref(box));work=screen['work']
            assert work['x']<=box.left<box.right<=work['x']+work['width']
            assert work['y']<=box.top<box.bottom<=work['y']+work['height']

def wait(predicate, label, seconds=20):
    until=time.monotonic()+seconds;last=None
    while time.monotonic()<until:
        guard()
        try:
            last=predicate()
            if last:return last
        except (RuntimeError,subprocess.TimeoutExpired) as error:last=str(error)
        time.sleep(.06)
    raise RuntimeError(f'{label}: {last}')

def ask(line):
    result=subprocess.run([str(binary),'--say',scene_file.stem,line],env=env,creationflags=flags,capture_output=True,encoding='utf-8',timeout=5)
    if result.returncode:raise RuntimeError(result.stderr)
    return result.stdout.strip()

def press(zone):
    result=ask('press '+zone);assert 'pressed' in result,result
    commands.append({'zone':zone,'result':result});guard()

def start_scene():
    global scene
    log=(output/f'scene-{len(report["stages"])}.log').open('wb')
    scene=subprocess.Popen([str(binary),'--scene',str(scene_file),'--screen',screen['name'],'--preview-monitor',screen['name'],
                            '--window-actions','--no-hud','--stall','0','--seconds','90'],env=env,creationflags=flags,stdout=log,stderr=subprocess.STDOUT)
    log.close();owned_pids.add(scene.pid)
    wait(lambda:ask('get win.docks.0'),'scene endpoint')
    wait(lambda:len(canvases(scene.pid))==1,'scene window')

def stop_scene():
    global scene
    if scene is not None and scene.poll() is None:
        try:ask('quit');scene.wait(timeout=8)
        except Exception:scene.kill();scene.wait(timeout=5)
    scene=None

def fixture_index():
    count=int(float(ask('get win.docks.0')))
    matches=[i for i in range(min(count,12)) if 'windows-dock-fixture' in ask(f'get win.dock.0.{i}.name').lower()]
    return matches[0]+1 if len(matches)==1 else 0

def launches():
    result=[]
    for path in output.glob('launch-*.json'):
        try:value=json.loads(path.read_text(encoding='utf-8'))
        except json.JSONDecodeError:continue
        assert value['monitor']==screen['name'];owned_pids.add(value['pid']);result.append(value)
    return result

def close_fixture(entry):
    pid=W.DWORD();user.GetWindowThreadProcessId(entry['hwnd'],C.byref(pid));assert pid.value==entry['pid']
    assert user.PostMessageW(entry['hwnd'],0x0010,0,0)
    wait(lambda:not windows(entry['pid']),'owned fixture close')

def capture(name):
    guard();hwnd=canvases(scene.pid)[0];box=W.RECT();assert user.GetWindowRect(hwnd,C.byref(box))
    width,height=box.right-box.left,box.bottom-box.top
    dc=user.GetDC(None);memory=gdi.CreateCompatibleDC(dc);bitmap=gdi.CreateCompatibleBitmap(dc,width,height);previous=gdi.SelectObject(memory,bitmap)
    try:
        assert gdi.BitBlt(memory,0,0,width,height,dc,box.left,box.top,0x00CC0020|0x40000000)
        gdi.SelectObject(memory,previous);previous=None
        info=Info();info.header=Header(C.sizeof(Header),width,-height,1,32,0,width*height*4,0,0,0,0)
        data=C.create_string_buffer(width*height*4)
        assert gdi.GetDIBits(memory,bitmap,0,height,data,C.byref(info),0)==height
        image=Image.frombytes('RGB',(width,height),data.raw,'raw','BGRX')
        path=output/(name+'.png');image.save(path)
        images.append(dict(file=path.name,sha256=hashlib.sha256(path.read_bytes()).hexdigest(),width=width,height=height))
    finally:
        if previous:gdi.SelectObject(memory,previous)
        gdi.DeleteObject(bitmap);gdi.DeleteDC(memory);user.ReleaseDC(None,dc)

try:
    initial=subprocess.Popen([str(fixture)],env=env,creationflags=flags,stdout=subprocess.DEVNULL,stderr=subprocess.DEVNULL);owned_pids.add(initial.pid)
    first=wait(lambda:next((x for x in launches() if x['pid']==initial.pid),None),'initial fixture')
    start_scene();index=wait(fixture_index,'dock metadata')-1
    time.sleep(.7);capture('01-populated')
    press(f'keep.{index}')
    pins=output/'config/wm/windows-dock.json'
    wait(lambda:pins.exists() and len(json.loads(pins.read_text())['pins'])==1,'persisted pin')
    wait(lambda:ask('get win.dock.0.0.pinned') in ('1','true'),'renderer pinned state');capture('02-pinned')
    close_fixture(first);initial.wait(timeout=5)
    wait(lambda:ask('get win.dock.0.0.windows')=='0','closed source retained as a pin');capture('03-pinned-closed')
    report['stages'].append('pin-survives-window-close')
    stop_scene();start_scene();wait(lambda:ask('get win.dock.0.0.pinned') in ('1','true'),'pin survives scene restart');capture('04-restarted')
    assert ask('get win.dock.0.0.windows')=='0'
    press('open.0')
    second=wait(lambda:next((x for x in launches() if x['pid']!=initial.pid),None),'real relaunch')
    wait(lambda:ask('get win.dock.0.0.windows')=='1','relaunched native window');time.sleep(.4);capture('05-relaunched')
    assert second['args']==[];report['stages'].append('pin-restarts-actual-native-program')
    press('unkeep.0');wait(lambda:json.loads(pins.read_text())['pins']==[],'persistent unpin');capture('06-unpinned')
    close_fixture(second);report['stages'].append('unpin-updates-storage-and-ui');guard()
    report.update(passed=True,foreground_owned_at_checks=False,commands=commands,launches=launches(),packaged_apps_tested=False,os_drag_drop_tested=False)
finally:
    stop_scene()
    if initial is not None and initial.poll() is None:initial.kill();initial.wait(timeout=5)
    report['remaining_owned_windows']=[pid for pid in owned_pids if windows(pid)]
    (output/'report.json').write_text(json.dumps(report,indent=2),encoding='utf-8')
print(json.dumps(report,indent=2))
