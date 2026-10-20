# Plan: the app and the service

Status: **done** (2026-10-20). The service owns the engine; the two fronts that used to
drive the launchers - the TUI's menu and the console's job runner - are clients of it
(`runner.rs`), and the tuner's own candidate restarts are too, now that the protocol carries
the caller's `GOLEM_*` overrides with the request. There is no launcher anywhere to hand
anything back to: the shell stack is deleted, and an engine whose adapter says it installs
itself is refused by name. What is still open is the tray and the logo (a GUI toolkit and an
asset the repo does not carry).

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

## The design, as built

The rule is one sentence: **the service is the only process that spawns the engine, and
every verb is a client of it.** That is what makes the pipe problem go away at the root
rather than being worked around - a client that spawns nothing cannot pass its handles to
anything - and it is what the container needs, because there the service is PID 1 and the
verbs have to work with no CLI at all.

- **One socket, one protocol.** `.local/golem-service.json` holds the port the service bound
  (ephemeral, so two checkouts never fight over one) and a token. Requests and replies are
  one JSON object per line: `{"token":…,"verb":"up"}` in, and
  `{"event":"line","text":…}` … `{"event":"done","ok":true,"code":0}` back.
- **Streamed, not collected.** A verb can install an engine and fetch 20 GB, and a command
  that shows nothing for ten minutes reads as a hang. The lines arrive as they happen, and
  the `done` event is the acknowledgement a caller waits for before it reports anything -
  which is also what keeps `golem up && golem bench` from racing.
- **The verbs are the same code.** `up`, `down`, `restart` and `status` take a sink instead
  of writing to stdout, so the CLI and the service run one implementation and neither is a
  second copy of the other.
- **The sink goes two places**: the client that asked, and the service's own stdout. A
  terminal asking for `up` gets the lines back; a container has no client, and there the
  service's stdout *is* the container's log.
- **Ctrl+C stops the engine.** The watcher is why the service needs a runtime at all. A
  service that died without stopping what it owned would leave a model resident with nothing
  pointing at it - the orphan that once cost a session's testing.
- **One verb at a time.** A second request while one runs is refused rather than queued;
  two `up`s at once would fight over one engine, one pid file and one card.

**Starting the service is the one spawn that remains**, and it is where the pipe question
survives in miniature: `up` starts it detached, and on Windows a *piped* first `up` would
wait on that one child. Once a service is running, nothing the CLI does can hold a caller's
pipe, so the deviation shrinks from "every `up`" to "the first `up` after a cold start" -
and the FFI option below closes even that.

## What is still open

- **A detached process, not a Windows service (SCM).** One implementation on all three
  platforms, no installer, and enough for "the engine is a child of something that outlives
  my terminal". SCM would buy start-on-boot and a Services entry, and cost a second code
  path that only exists on one platform; a tray app or a login item gets the same result.
- **The tray and the logo are the open question**, and they are presentation on top of all
  of this rather than part of it: a tray needs a window/GUI toolkit - a new dependency, which
  this repository has always decided deliberately - and a logo is an asset it does not carry.
  Neither is needed for the service to be worth having, and both can land after it.

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

1. **The service** - `service.rs`, built: the endpoint file and token, the line protocol, the
   streamed sink, `up` / `down` / `restart` / `status` / `shutdown`, the Ctrl+C watcher (and,
   for a container's PID 1, SIGTERM), and the one-verb-at-a-time rule. The CLI is the client
   of it: `up` / `down` / `restart` send verbs, `up` starts a service when there is none.
2. **The app** - built (2026-10-20, `runner.rs`): the TUI's menu and the console's job runner
   ask the same runner, which asks the service for the engine verbs and runs the binary's
   own verbs for the rest. Verified live through the console: a `status` spawns the service
   on demand and its lines reach the page's job pane; `start` brings the router up; `stop`
   takes the engine down and leaves the service; a folder with no launcher runs the whole
   menu, which is the shape a release folder has.
3. **Then the tray and the logo**, which are presentation over the same snapshot
   (`api/state` is already a plain struct, which is what `tui.rs` was built to prove).

## The last launcher path, closed

`tune` used to drive the launchers for its own work: every candidate restart and every gate
eval was a child process with the `GOLEM_*` overrides in its environment. That is done - a
candidate restart is a service request carrying those overrides, and the gate runs this
binary's own `eval` as a child of the tuner. The protocol learned the one thing it was
missing to make that possible: a request may carry an `env` object of `GOLEM_*` pairs, set for
that one verb and put back afterwards, so one request's candidate preset cannot decide the
next request's engine. It also fixed a bug that predated the tuner work: `GOLEM_MODELS_PRESET=x
golem up` was silently ignored, because the service runs a verb in its own environment and a
caller's cannot reach it by inheritance.

Nothing here blocks [plan-launchers.md](plan-launchers.md) or
[plan-portable-binary.md](plan-portable-binary.md) except the pipe, which is recorded here
so it is not rediscovered: the launcher port ships with it, and the service is what removes
it.
