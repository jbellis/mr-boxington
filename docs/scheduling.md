---
description: Run concurrent Cargo builds with separate target directories and one shared compiler budget.
---
# Parallel builds

Give independent Cargo commands their own target directories. mbx coordinates
their real compiler processes through a shared CPU and memory budget; Cargo
continues to plan dependencies within each build.

`check` and `clippy` need no setup. In a managed target directory, they
already write to their own [check lane](/managed-targets#check-lanes), so
`mbx clippy` runs beside `mbx build`. The rest of this page is for commands
that would otherwise share a target directory, such as two builds or several
lint configurations.

## Run independent tasks

Any task runner can start multiple mbx commands at the same time. For example,
mise runs these two lint configurations together:

```toml
# mise.toml
[tasks."lint:default"]
run = "mbx clippy --workspace -- -D warnings"
env.CARGO_TARGET_DIR = "target/clippy-default"

[tasks."lint:all"]
run = "mbx clippy --workspace --all-features --all-targets -- -D warnings"
env.CARGO_TARGET_DIR = "target/clippy-all"
```

```sh
mise run lint:default ::: lint:all
```

Separate target directories keep Cargo's directory lock from serializing the
commands. mbx shares one machine-wide compiler pool and deduplicates identical
work in flight without further configuration. See
[Machine-wide scheduling](/how-it-works#machine-wide-scheduling) for the
mechanism and [Parallel Cargo steps](/github-action#parallel-cargo-steps) for
the same setup in GitHub Actions.

## Machine-wide compile scheduling

Each Cargo process sets its own concurrency. Without coordination, several
builds can collectively start more compiler processes than the machine can
comfortably run. mbx coordinates them through a shared pool under the cache
directory. Scheduling is on by default; `MBX_SCHEDULER=0` disables it.

A permit represents a share of the pool's CPU and memory budget. The pool has
`scheduler.cpus` permits (default: logical CPUs), minus
`scheduler.reserve_cpus` (default: 0), with at least one permit remaining.
`scheduler.memory` defaults to 85% of physical memory. In a Linux container,
the cgroup's memory limit constrains that budget.

Compiler processes take permits according to their estimated memory use.
Unmeasured compilations start at one permit; native links start at two and can
use estimates from earlier links. Measurements refine later admissions. mbx
measures compiler memory on Unix only, so on Windows, compilations keep these
starting weights. Set `scheduler.memory = "none"` to use CPU permits without
memory weighting.

For non-link compiles of a crate already measured above one memory permit,
mbx learns separate peaks for each set of emitted outputs, crate types, and
explicit target. Repeated emit and crate-type flags are combined, sorted, and
deduplicated; output paths and dep-info do not distinguish modes. With no
emit flag, rustc emits a link output. The mode also includes test compilation,
optimization, LTO, codegen units, debug info, incremental compilation
(including flags added by mbx), and explicit compiler thread settings. Clippy
compiles learn their own estimates, apart from `cargo check`. A compile with
more than one `--target` matches no single-target mode.
After three successful samples of a mode (the stored count is capped at
three), its estimate is the mode's highest peak plus 50% headroom, capped at
the crate-wide highest peak. Until then, mbx uses the crate-wide peak. Peaks
never decrease; failed compiles and compiles run through another wrapper,
such as a `RUSTC_WRAPPER` cache, do not count as successful samples, and OOM
kills raise both peaks. An older shim
that drops sample counts makes modes return to the crate-wide fallback.

`mbx stats --scheduler` shows scheduler peaks, successful sample counts, and
compile charges in readable sizes and permits, with their mode or crate-wide
fallback source. Link and test-run entries show recorded peaks and defer
charges to admission. Add `--json` for raw byte counts. Without `--scheduler`,
the stats output omits this section. Its charges use the global scheduler
settings; a workspace `.mbx.toml` can change them for that workspace's builds.
The read-only activity snapshot shows each lease and waiter with its name, mode
key, build ID, pressure domain, age, and the charge it was really admitted or
queued with. Older records show unknown names; abandoned records can remain
visible until an admission reclaims them.

With memory weighting enabled, completed admitted compiles, links, test binaries,
and nextest tests append estimate-accuracy observations to
`<cache_dir>/scheduler/observations.jsonl`. The existing `events` setting gates
this log (on by default); `events = false` disables observations too. Each line
pairs the prediction and its source with the admitted permits, bytes per permit,
measured child peak RSS (or null), outcome, wrapper status, and wait/run times.
Observations include build identity and applicable build and domain capacities.
`mbx reserve` records each reservation's weight, wait and run times, and command
outcome; reservations have no peak measurement.
Peak RSS has the same child-process measurement limitations as the memory ledger.
Lines are smaller than 1 KiB; exceptionally long names, keys, or pressure domains
are shortened with a hash suffix to preserve distinct identities.

Before appending to a file over 8 MiB, a writer tries `observations.lock` without
waiting, then rotates to `observations.jsonl.1`, replacing the previous generation.
The usual retention bound is two approximately 8 MiB files. A busy rotation lock
can temporarily allow growth; concurrent appends and rotation can lose a few lines.
Observation failures are debug-logged and do not prevent the workload from running.

The estimate-accuracy section of `mbx stats --scheduler` reads both generations,
ignoring malformed lines, and shows the retained time span as UTC dates and
times. Timestamp fields in `--json` are Unix milliseconds.
It shows total observations, reservation exceedances, the worst peak/reservation
ratio and its timestamp, and the 20 keys with the highest worst-case ratios.
`--json` includes all keys, counts, highest peaks, median and nearest-rank p90
peak/prediction ratios, and average reserved-but-unused bytes (clamped to zero
when a run exceeds its reservation). Failed and OOM-killed runs have separate
counts and still contribute to exceedances and worst cases; median, p90, and
unused-byte averages use only successful unwrapped measurements. Missing peaks
count as observations but do not contribute to measured statistics. Without
observations, this section shows a short empty-state line.

When a compilation cannot fit, it queues before newer requests, so a large
compilation still runs while other builds keep submitting small ones. Newer
requests may use capacity a waiting request cannot use when it next becomes
able to start. Within one scope this never delays an older request. When a request
waits on its own build or container limit, the existing rule already lets
other builds or containers start, which can rarely delay the waiting request
after a pass. Cancelling a waiter or killing its process releases its queue
position.

Cache hits do not need compiler permits. If a process dies, the kernel
releases its permits. For the weighting and recovery details, see
[Machine-wide scheduling](/how-it-works#machine-wide-scheduling).

Live pressure control is on by default (`scheduler.pressure = true`). It
pauses further admissions, including compilations with no memory history, while
available memory is low or memory stalls show sustained pressure. The signals
mbx reads depend on the platform:

- Linux: available-memory headroom and pressure stall information (PSI), the
  kernel's measure of time lost waiting for memory
- macOS: available-memory headroom only
- Windows: neither, so pressure control does not pause admissions there

At least one compilation can run when the pool is idle. After five healthy
seconds, mbx admits at most one compilation every half second for five seconds,
then lifts that limit. Pressure control does not suspend running processes. To
freeze them, see
[Experimental Linux compiler supervision](#experimental-linux-compiler-supervision).

Containers that share a cache directory share permits, but mbx measures each
container's pressure from its own memory readings and pauses only that
container's admissions. Suspended work is the exception: while compiler
supervision in any container has frozen a compilation, every container sharing
the cache waits for it to resume.

When containers share a cache directory, configure `scheduler.cpus` and
`scheduler.memory` in their shared configuration using the host's budget.
Otherwise each container defaults the shared pool to its own limits. For
example, a host budget of 64 CPUs and 256 GiB is:

```toml
[scheduler]
cpus = 64
memory = "256GiB"
```

mbx also enforces a separate ceiling for each container. The ceiling is the
container's available CPUs and, when the configured memory budget exceeds the
container's memory limit, 85% of that limit. A memory budget that fits within
local memory applies as configured. All builds in that container share the
ceiling; other containers draw from their own ceilings and the common host pool.
Pressure control remains local to each container. During an upgrade, older
compiler leases without a container identity count against every container
until they finish. Patched wrappers also account for the permit units of builds started
before a budget change, conservatively preserving their CPU charges. Upgrade
all wrappers sharing the pool to get ordered admission.

Set `scheduler.pressure = false` (`MBX_SCHEDULER_PRESSURE=0`) to admit
compilations on permits and memory estimates alone. Disabling the scheduler or
setting `scheduler.memory = "none"` also disables pressure control. If the
pressure probes fail, scheduling falls back to the same permit and estimate
checks.

Use `scheduler.priority = "low"` (`MBX_SCHEDULER_PRIORITY=low`) for an editor's
background check or CI on a shared machine. While normal-priority work is
waiting, low-priority builds leave a quarter of the pool available for it.

## Reserve capacity for external work

Use `mbx reserve` for a command whose work consumes the same host CPU or
memory but does not run through mbx, such as a build in a VM or container:

```sh
mbx reserve --memory 6GiB --cpus 4 -- ./run-windows-build-in-vm.sh
```

The command waits until the requested capacity is free, then a dedicated worker
holds it until the external command exits. Killing the invoking `mbx reserve`
process alone does not release capacity while that worker still waits for the
command; a crashed worker does release its lease through the operating system.
The command must wait for work it launches: detached background processes can
outlive the reservation and are not covered after the command exits.
`--memory` is measured against `scheduler.memory`; omit it when memory
scheduling is disabled. Use `--priority low` for background work that
should yield at admission time to waiting normal-priority builds; an admitted
reservation, like an admitted compiler, is never preempted.

## Schedule test binaries

By default, permits cover compilers only. When several test commands reach
their test runs together, each libtest harness can start threads across the
machine. Set `scheduler.tests = true` (`MBX_SCHEDULER_TESTS=1`) to run test
binaries through the same pool:

```sh
MBX_SCHEDULER_TESTS=1 mbx test --workspace   # this run
mbx settings set scheduler.tests true        # every run from now on
MBX_SCHEDULER_TESTS=1 mbx nextest run        # schedule each nextest test
```

mbx becomes Cargo's target runner for `cargo test` and `cargo nextest`, and
calls any runner you have configured. Cargo test binaries wait for permits as
a suite; nextest waits separately for each test. Listing tests does not take a
permit.

For Cargo test, each test binary waits for permits before it starts:

- Once a binary has been measured, it asks for the average number of cores it
  kept busy: CPU time divided by wall time, rounded to the nearest core.
- Before that, `--test-threads=N` or `RUST_TEST_THREADS=N` asks for `N`
  permits, and otherwise the binary asks for half the pool, rounded up.
- A binary whose measured memory needs more permits takes that many instead.

For Cargo test, only complete suite runs are measured; a run narrowed by a
test name, `--skip`, or `--ignored` is not. A run under a second is too short
to average, so the binary is recorded as one core. A recorded core count only goes up,
because a suite measured on a busy machine gets fewer cores than it would use.
mbx measures CPU and memory on Unix only. On Windows, a Cargo test binary asks
for its stated thread count or half the pool; an unmeasured nextest test asks
for one core and, when memory scheduling is on, two memory permits.

History is kept per Git repository, package, and test binary. Worktrees of one
repository share it; separate clones and unrelated projects do not. Outside
Git, projects that share a package and test name share history. A run with a
stated thread count keeps its history apart from runs at the default thread
count.

Nextest history is kept separately for each Git repository, nextest binary ID,
and test name. A test with history uses its own measurements. A new test uses
the largest measurements of other tests in that binary; when the binary has
no history, it starts with one core and, when memory scheduling is on, two
memory permits. Successful tests record their measured peak even when they use
less than one permit, so later runs can use a smaller estimate. On Unix, peak
memory is the largest single process reported by `rusage`; a descendant's
peak is folded in when it exits, but concurrent child processes are not added
together.

nextest starts a test's `slow-timeout` clock, including any `terminate-after`
limit, when it starts mbx's runner. Time spent waiting for a permit therefore
counts as test time, and nextest offers runners no way to pause the clock. A
test that waits can be reported as SLOW, and with `terminate-after` it can be
killed before it starts. After one second of waiting, mbx writes a note to the
test's stderr, which nextest shows beside the report. If your nextest profile
sets `terminate-after`, leave room in it for queueing, or lower nextest's
`--test-threads` so fewer tests queue at once.

Builds a test starts, such as trybuild or compile-fail suites, are charged to
the test's permits and run without taking permits of their own.

These run unscheduled:

- doctests
- `cargo test --no-run`
- nextest test listings
- commands with `--config`, a `+toolchain` override, or a directory change
  (`-C`, `--directory`)
- tests run from `--archive-file` when their binaries target a triple other
  than the requested or host target
- test binaries run directly, without `cargo test` or `cargo nextest`
- other test runners

## Choose the scope of a limit

| Limit | Affects |
| --- | --- |
| `scheduler.cpus`, `scheduler.memory` | The shared compiler pool |
| `scheduler.reserve_cpus` | Capacity left outside that pool |
| Cargo `-j` or `CARGO_BUILD_JOBS` | How much of the pool one build may hold |
| `scheduler.priority = "low"` | Whether a build yields to waiting normal-priority work |
| `scheduler.tests = true` | Whether Cargo test binaries and nextest tests take permits |

A memory budget schedules work using measurements; it is not an operating-system
memory limit. A compiler process can still exceed its estimate. For laptop
settings, see
[Keep a laptop responsive](/cookbook/local-development#keep-a-laptop-responsive).

## Experimental Linux compiler supervision

Pressure control only holds back new compilations. With supervision, mbx can
also freeze running compilers while memory is under pressure and resume them
when pressure recovers.

::: warning Experimental
Compiler supervision is experimental and off by default. It needs Linux and a
delegated cgroup v2 directory. If you enable it where delegation is unavailable
or on another platform, mbx warns once per build and continues with admission
scheduling.
:::

`scheduler.suspend = true` (`MBX_SCHEDULER_SUSPEND=1`) opts compiler processes
into cgroup supervision. Supervision also needs:

- `scheduler.cgroup_root` (`MBX_SCHEDULER_CGROUP_ROOT`) set to the absolute
  path of a writable, delegated cgroup v2 directory
- pressure control and memory scheduling, which are both on by default

Set `scheduler.suspend` and `scheduler.cgroup_root` in your global
configuration file or the environment. A repository's `.mbx.toml` cannot opt a
contributor into supervision or choose their delegated directory. mbx creates
its own cgroups inside the delegated directory and does not change limits on
it. Enabling supervision does not provision systemd units, grant permissions,
or modify host cgroup limits.

```toml
[scheduler]
suspend = true
cgroup_root = "/sys/fs/cgroup/my-delegated-builds"
```

After pressure persists for two seconds, mbx can freeze the newest eligible
compiler tree (the compiler and the processes it starts), at most one per
second. The oldest eligible compilation keeps running; when it finishes, mbx
resumes its successor to preserve progress. After pressure recovers, mbx
resumes suspended work oldest first, before it admits anything new. Frozen
compilers keep their permits. Freezing stops execution but keeps allocated
memory, so it cannot rescue a compilation that is too large to run alone.

Custom compiler wrappers, build-script binaries, test binaries, and compilers
nested inside a supervised compiler are not eligible.

mbx runs a supervisor and an independent watchdog outside the compiler cgroups.
Each watches the other's heartbeat, and the supervisor also checks its
pressure probes:

- If the supervisor's heartbeat stops, the watchdog thaws the supervisor's
  compilers and disables further suspension.
- If the watchdog's heartbeat stops, the supervisor does the same and starts a
  replacement watchdog for the compilers still running.
- If a pressure probe fails, the supervisor thaws its compilers and disables
  further suspension.

After suspension is disabled, mbx keeps cleaning up the cgroups it owns until
their compilations finish. Once an earlier supervisor's compilers have exited,
the next supervisor removes the cgroups and state it left behind.

When a compilation is cancelled, mbx thaws its cgroup before terminating any
leftover processes. Suspension itself never kills and retries a compilation;
mbx terminates orphaned descendants only to clean up after a cancellation.

mbx records suspend and resume events, plus each compilation's peak memory and
suspended time, under `scheduler/supervision-*/` in the cache directory. When a
suspended compilation finishes, mbx reports how long it spent suspended.
