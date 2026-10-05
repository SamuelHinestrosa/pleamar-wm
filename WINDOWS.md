# Native Windows backend (in development)

Windows support now belongs to pleamar-wm as well as pleamar and Marea. This
checkout starts the native desktop companion; it does **not** yet provide a
complete Windows equivalent of the Linux compositor session. DWM continues to
compose applications. No WSL, Wayland server or Unix shell is required.

## Build and run from PowerShell

Install the stable Rust x64 MSVC toolchain and Visual Studio Build Tools with
Desktop development with C++ and the Windows SDK. Keep this checkout next to
the matching `pleamar` Windows port (`../pleamar`). The current CI pins pleamar
commit `7c0edebd8e1c97c6c590f463ea58454e189ce55d` from the Windows PR; upstream
pleamar alone does not yet include that backend.

```powershell
cargo build --release --locked
cargo test --release --locked
./target/release/pleamar-wm.exe capabilities
./target/release/pleamar-wm.exe monitors
./target/release/pleamar-wm.exe windows
```

Luau stays enabled through the default pleamar dependency. `--scene FILE`
starts a native pleamar scene with its normal hot reload. Ship the same
verified graphics runtime DLLs as pleamar when distributing this executable.
There is no Windows installer or automatic startup for this companion yet.

## Explicit window layouts

The current backend manages only explicitly selected normal, resizable
application windows. It ignores desktop surfaces, tool windows and windows
on other virtual desktops. It does not automatically rearrange applications,
replace Explorer, intercept keys, or implement its own compositor input seat.

Get IDs from `windows` and a monitor name from `monitors`, then:

```powershell
# Replace these placeholders with current catalog IDs.
./target/release/pleamar-wm.exe tile '\\.\DISPLAY2' grid --save './positions ñ.json' ID1 ID2
./target/release/pleamar-wm.exe restore-layout './positions ñ.json'
./target/release/pleamar-wm.exe window ID1 minimize
./target/release/pleamar-wm.exe window ID1 restore
```

`left`, `right`, `columns`, `rows` and `grid` use the chosen monitor's work
area, physical coordinates and DPI-scaled gaps. Windows must already be
wholly on that monitor; maximized/minimized windows must first be restored.
The undo file must not exist. Original positions are saved and flushed before
moving anything. A rejected geometry triggers rollback and a nonzero exit;
the recovery file is retained. Restoring refuses disconnected monitors,
closed windows and windows that moved to a different monitor. An application
can constrain its size or refuse movement; these are reported as failures.

Position and state changes preserve focus and Z order. IDs include native
handle, process, thread and process creation time; they are checked again at
each operation. They are short-lived desktop references, not persisted app
identities or an authorization boundary. Do not reuse an old catalog across
application restarts.

## Status and remaining parity work

| Area | Windows status |
| --- | --- |
| Monitor/catalog queries and `hyprctl` compatibility reads | Native implementation |
| Explicit five-layout arrangement, undo, minimize/restore | Passed native tests with three owned windows on DISPLAY2; broad application acceptance pending |
| Pleamar scenes, Luau and hot reload | Native D3D12 scene, Luau callbacks and saved logic/scene reloads verified on DISPLAY2 |
| Automatic per-monitor session, rules and live scene layouts | Pending |
| Marea menu/launcher integration and package inclusion | Pending; do not set `in_wm` merely because the executable exists |
| Rain, snow, ride, dock, animated window transitions | Pending native equivalents |
| Per-monitor tide pools and overview | Pending; Windows virtual desktops are not the same model |
| Independent agent pointer/keyboard, glow and stop UI | Pending; Marea currently uses guarded shared Windows input |
| Remote desktop/WebRTC and sharing integration | Pending native capture/input/encoder adapters |
| DRM, libinput, PipeWire, Wayland protocols and login session | Linux components; Windows owns the corresponding system facilities |

Unavailable WM commands exit with an error. `capabilities` states their status
explicitly; no rain, independent input seat or compositor session is simulated.
The existing Marea installation is not changed by building this checkout.

## Native acceptance (separate from CI)

The opt-in test creates three of its own ordinary native windows only on an
explicitly named **non-primary** display. It tests all five arrangements,
physical bounds, undo, Unicode names and paths, minimize/restore, stale IDs,
focus preservation and failure cases, then destroys its windows. It sends no
mouse or keyboard input. Close any applications that might interfere with the
chosen secondary display before running it:

```powershell
$env:PLEAMAR_WM_TEST_MONITOR = '\\.\DISPLAY2'
cargo test --release --locked native_layouts_and_restore_on_secondary_monitor -- --ignored --nocapture --test-threads=1
```

The CI workflow is prepared to build the Windows and Linux targets and run
ordinary unit tests; it has not been executed for this new companion yet. The
interactive test stays ignored in CI. Neither compilation nor the geometry
test proves completed visual effects, interaction parity or a complete WM port.

On 2026-10-05, the release build with default Luau support succeeded on Windows
x64/MSVC. Six ordinary unit tests passed. The separate native test passed on
`\\.\DISPLAY2`: all five layouts and their undo, actual minimize/restore,
Unicode paths, closed-window rejection, and rollback after a test application
rejected its new size. It created three owned windows, sent no physical input,
left foreground focus unchanged, and closed all three windows. This does not
validate other applications, mixed-DPI hotplug or the complete compositor UI.
The executable also rendered a real scene on DISPLAY2 through D3D12. Five
captures checked startup, a Luau event, a saved Luau change, a saved scene
change and a subsequent callback. The counter reached seven, accented text
rendered correctly, and the process closed successfully without changing
foreground focus. This was passive validation, not a physical input test.
Linux execution and the new WM CI remain pending.
