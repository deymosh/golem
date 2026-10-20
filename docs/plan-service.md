# Plan: the app and the service

Status: **direction, 2026-10-20, from the user**, not started. One item here is not a
direction but a measured defect in the launcher port; the rest is the shape the user asked
for.

## The measurement that makes this concrete

`golem up` spawns the engine and returns as soon as `/health` answers. On Windows, it also
leaves the caller's stdout pipe open **for as long as the engine runs**, so
`./golem up | tail` never finishes.

The cause is not our code being careless, and it is worth writing down because the obvious
fixes are wrong:

- `std` passes `bInheritHandles = TRUE` to `CreateProcess` as soon as one std handle is set.
  The child's own handles point at NUL, so nothing looks wrong from the inside - but it
  inherits *every* inheritable handle we hold, and an MSYS or PowerShell pipe is created
  inheritable.
- Setting **no** stdio is worse, not better: with `hStdXxx` left null and handles inherited,
  the child gets our handles outright and writes its banner into our output.
- `cmd /c start "" /b ...` does not help, and neither does `cmd /c start ""` with a new
  console. Measured, all three: the pipe is held.

Only a process created with a clean handle table escapes it, which is exactly what
PowerShell's `Start-Process` does - and why the shell launcher never had this problem. In
safe Rust there is no way to ask for it: `unsafe_code` is `forbid` workspace-wide, so the
two FFI calls that would clear `HANDLE_FLAG_INHERIT` on our stdout before spawning (and put
it back after) are not available either.

Measured, on this box:

| spawn | piped `up` returns |
|---|---|
| `Stdio::null()` ×3 (shipped) | when the engine stops |
| no stdio configured | when the engine stops, and the banner lands in the caller's pipe |
| `cmd /c start "" /b ...` | when the engine stops |
| PowerShell `Start-Process -WindowStyle Hidden` | immediately |

What it does **not** break: `golem up` typed by a human (a console handle is not a pipe),
and anything that sends the output to a file - the console's job runner, a CI log, a
`> out.txt`. Those return in about a second.

## The shape the user asked for

The direction, as raised (2026-10-20): golem should own the engine's pid as a child, so that
killing golem takes the engine with it and the two cannot get out of step; the binary should
start the service and open the TUI when it is run; it should carry the golem logo, and sit
in the Windows tray like other apps do. The shape underneath all of that is the one a
desktop app plus a service has: the app is the face, the service owns the engine, and the
app drives the service rather than the engine.

That is one design, and it is a good one: **a long-lived service owns the engine; an app
talks to the service.** The pieces:

| today | becomes |
|---|---|
| `up` spawns `llama-server` and exits | the service spawns it, holds it, and outlives it |
| the pid file is the only record of what runs | the service is the record; the pid file is a courtesy |
| a killed tune leaves an orphan holding 11.7 GB of VRAM | the engine is a child of the service, so it goes when the service goes |
| the console, the TUI and the CLI each talk to the engine | they talk to the service, which owns the one connection |
| a piped `up` holds the caller's pipe | nothing spawns on a CLI's behalf any more |

**The service does not fix the pipe by itself**, and this is worth stating before anyone
builds it expecting that. The inheritance happens at whichever `CreateProcess` sets
`bInheritHandles = TRUE`, so a service *started by a piped `up`* inherits the pipe just as
the engine does today - and because the service is long-lived, it would then hold it for
longer than the engine did. The pipe is fixed only when the process that spawns has a clean
handle table, which is a different question from who owns the engine.

That leaves three ways, and one of them has to be chosen:

- **Clear the inherit flag before spawning** - two FFI calls (`GetStdHandle`,
  `SetHandleInformation`, and back again), about fifteen lines. It fixes the pipe exactly,
  in every case, with no shell involved. It is `unsafe`, and the workspace forbids `unsafe`
  outright, so choosing it means relaxing that lint to `deny` plus one `#[allow]` on one
  function. This is the only option that removes the deviation rather than routing around
  it.
- **Have PowerShell `Start-Process` do the spawning** - the one form measured to release the
  pipe. It works and it is what the launcher has always relied on, but it puts PowerShell
  back on the path the port exists to clear, and it would be there only for this.
- **Accept it and say so.** Cost: a piped `up` returns when the engine stops. A typed one,
  and one whose output goes to a file, return in about a second. The port already ships this
  way, and `docs/plan-launchers.md` records it as the one place it does not behave like the
  launcher.

Independently of the pipe, the service is still the right shape - it is what the container
needs, what the tray needs, and what stops a killed tune from leaving an orphan holding the
VRAM.

## What has to be decided

- **A real Windows service (SCM) or a plain detached process?** A detached process is one
  implementation on all three platforms and is enough for "the engine is a child of
  something that outlives my terminal". SCM gives start-on-boot and a Services entry, and is
  Windows-only. The cheap answer is the detached process plus a tray app that starts it.
- **How the app finds the service.** A loopback socket with a tiny JSON protocol reuses what
  `web.rs` already has; a named pipe is the Windows-native answer and needs a second
  implementation. `golem web`'s `api/state` is already the shape of the payload.
- **Autostart.** On demand (the app starts the service if it is not there) is the least
  surprising and needs no installer. A tray icon implies "it is always running", which
  implies autostart.
- **The tray and the logo are new dependencies**, and that is a decision this repository has
  consistently made deliberately: a tray needs a window/GUI toolkit, and a logo is an asset
  the repo does not carry. Neither is needed for the service to be worth having, and both
  can land after it.

## The third thing that wants this

[plan-deploy-modes.md](plan-deploy-modes.md): an image whose entrypoint is golem cannot be
`ENTRYPOINT ["golem", "up"]`, because `up` returns as soon as the engine is healthy - the
container would exit the moment it became useful. PID 1 has to be a process that owns the
engine and stays alive, which is this one. So the service is what a tray app talks to and
what a container needs to be a container, and it is the reason to build it first.

Note that inside a container the pipe question does not arise at all: holding stdout open is
what keeps `docker logs` alive, so it is the behaviour you want there. The pipe is a Windows
desktop problem, and the three ways out of it are above.

## Order

1. **The service.** One process that owns the engine: start, stop, restart, health, the pid,
   and the engine's log. `up` / `down` / `restart` become verbs it answers; the CLI and the
   console stop spawning anything.
2. **The app.** `golem` with no arguments already opens the TUI; make it ask the service
   instead of driving launchers, and make it start the service when there is none.
3. **Then the tray and the logo**, which are presentation over the same snapshot
   (`api/state` is already a plain struct, which is what `tui.rs` was built to prove).

Nothing here blocks [plan-launchers.md](plan-launchers.md) or
[plan-portable-binary.md](plan-portable-binary.md) except the pipe, which is recorded here
so it is not rediscovered: the launcher port ships with it, and the service is what removes
it.
