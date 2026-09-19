# Phone checks

Written while the Galaxy Z Fold6 was off the network, and **run on 2026-09-17**
once it came back (`192.168.1.93:34347`, SM-F956B, arm64-v8a, Android 14, cover
display 968x2376, app built at targetSdk 36, model `qwen/qwen3.8-27b` served by
LM Studio at `192.168.1.10:1234`).

**Result: 10 of 11 passed; check 10 is the one left, because it needs someone to
physically unfold the phone.** Two real bugs were found and fixed on the way -
see checks 2 and 3. Outcomes are recorded under each check; keep appending
rather than clearing them.

Reconnect with:

```sh
. ./env.sh
adb connect 192.168.1.93:41369      # the port changes when the phone reboots
apps/android/run-app.sh --logs
```

Each check says what is being tested, the steps, and what "pass" looks like.
Record the outcome here — this file is the device log, not a checklist to
throw away. See `docs/EXEC-PROBE.md` for the results format used earlier.

---

## 1. External commands under proot (arm64) — PASSED 2026-09-17

**Why the emulator cannot answer this.** On x86_64 Android, musl's `fork()` uses
`SYS_fork`, which the Android seccomp filter allows only for `lp32` processes —
so anything the guest shell spawns dies. On arm64, musl uses `clone()` and the
filter permits it. Every check involving a real command therefore has to run on
the phone. See `docs/EXEC-PROBE.md` (Fold6, 2026-09-16).

**Steps.** Chat: `run "uname -a && ls /" and tell me what you see`.

**Pass.** The tool card shows the real output and `ok`, not a fork error.

**Result.** Two `bash` calls, both `ok`:
`Linux localhost 6.1.128-android14-11-31998796-abF956BXXS3CZC5 … aarch64 Linux`
and the Alpine root listing. `fork()` is fine on arm64, as expected.

## 2. Command timeout kills a runaway — FAILED, FIXED, PASSED 2026-09-17

**Steps.** Chat: `run "sleep 600" with a 5 second timeout`.

**Pass.** Within ~5 s the tool card turns `failed` and its output ends with the
timeout notice. `adb shell run-as net.pedrosoares.mobilecoder ps` (or `top`)
shows no leftover `sleep`. The chat stays usable afterwards.

**Result — this is what the check was for.** First run: the tool never returned
at all, and `sleep 600` was still running 54 seconds later with `PPID 1`.

Two things were wrong, both invisible on the emulator:

- Killing the process we spawned does not kill what it started. The code (and
  its comment) assumed proot's `--kill-on-exit` covered this; it does not. That
  flag kills the guest tree when the *guest's* first process exits, and does
  nothing when proot itself is killed — its tracees are just detached.
- Because the orphan still held the write end of stdout, the reader task never
  saw EOF, so the call sat there long after the deadline had passed.

Fixed in `mc-sandbox`: the command is spawned into its own process group and the
deadline sends `SIGKILL` to the group, and the drain after a kill is bounded
(`DRAIN_GRACE`) so a survivor can never hang a turn again. Regression test:
`a_deadline_kills_what_the_command_left_running`.

Rerun: `failed` after 5.07 s, no leftover process, turn completed normally.

## 3. Large output does not hang the turn — FAILED, FIXED, PASSED 2026-09-17

The pipe-buffer deadlock (fixed by draining stdout/stderr concurrently in
`mc-sandbox`) needed >64 KB of output to show itself, and the regression test
only proves it on the host.

**Steps.** Chat: `run "yes hello | head -c 2000000"`.

**Pass.** The turn finishes in seconds, not at the timeout. The card shows the
head of the output and the "… N characters trimmed from the middle …" marker.

**Result — a second real bug.** `yes hello | head -c 2000000 | wc -c` hung: `head`
and `wc` finished, `yes` kept running. The same pipeline run by hand under the
same proot took 1.3 s, which is what made it findable — the difference was the
process doing the spawning, not the sandbox.

`yes` was not dying of SIGPIPE because SIGPIPE never arrived:

- Rust ignores SIGPIPE process-wide at startup, and an *ignored* disposition
  survives `exec`.
- An Android app's threads also run with several signals *blocked*, SIGPIPE
  among them, and a blocked signal survives `exec` too. Resetting the
  disposition alone was not enough — `/proc/<pid>/status` on the phone showed
  `SigBlk: 0000000080001204` (bit 12 = SIGPIPE) on every guest process.

Either way the write returns `EPIPE` instead of ending the process, and busybox
`yes` loops on a write error forever. Fixed with a `pre_exec` that clears the
signal mask and restores the default SIGPIPE; the same gap existed in the PTY
path and is now patched in `patches/teletypewriter` as well. Regression test:
`a_command_does_not_inherit_rusts_ignored_sigpipe`.

Rerun: 1.1 s, answer `2000000`, nothing left running. And with the output kept
raw (`seq 1 200000`), the model reported receiving lines 1–4221 and
198572–200000 with "… 1258895 characters trimmed from the middle …" between
them — the clamp working exactly as designed, and legible to the model.

## 4. Stop, on a real turn — PASSED 2026-09-17

**Steps.** Ask for something long (`write a 500-line C program, explaining each
section`). While it streams, press the red **Stop**.

**Pass.** Streaming halts within about a second, the transcript keeps what had
arrived, and a "Stopped." notice follows. Send another message: it runs
normally (the cancel token resets per turn).

Also stop *during a command*: `run "sleep 120"`, then Stop. The tool card should
end `failed`/stopped and the process should be gone.

**Result.** Tested in the hardest form, against the hung pipeline from check 3
before it was fixed: Stop ended the turn `cancelled` within about a second of the
tap, the tool reported "Not run: the user stopped the turn", and proot, the
shell and `yes` were all gone — the process-group kill from check 2 doing its
job. The next prompt ran normally.

## 5. Session survives being killed — PASSED 2026-09-17

**Steps.** Have a short conversation. Then:

```sh
adb shell am force-stop net.pedrosoares.mobilecoder
adb shell am start -n net.pedrosoares.mobilecoder/.MainActivity
```

**Pass.** The transcript is there on relaunch, and a follow-up ("what did I just
ask you?") shows the model kept the context, not only the view.

Interrupted tools: force-stop *while a tool is running*, relaunch, and the
restored card should read as interrupted rather than still running.

**Result.** Survived an app replacement (`adb install -r` kills the process) and
several force-stops: the transcript came back complete, tool cards and markdown
included, and follow-up questions showed the model still had the context. A turn
that was in flight when the app died is lost, as designed — the save happens per
turn.

## 6. Settings, from the phone alone — PASSED 2026-09-17

The point of this screen is to make a computer unnecessary, so it has to be
tested with adb doing nothing.

**Steps.** Tap **⚙** in the app bar. Set an endpoint and model (or a key). Save.
Send a message.

**Pass.** The turn uses the new endpoint (`adb logcat -s mobile-coder` shows
`[turn] endpoint …`) with no restart. Reopen ⚙: the endpoint and model are still
filled in, and the key field shows the stored-key hint rather than the key.
**Forget key** followed by a message gives the "No model is configured" error.

**Watch for.** The key must appear nowhere in `adb logcat` except as the
redacted `sk-ant-a… N chars` form.

**Result.** The form opened over the chat, kept the stored endpoint and model,
and took typing (the message box steps aside for it — see the emulator note
below). Changing the model to `qwen3.8-flash-next` and sending a message used the
new model on that very turn, with no restart: `[turn] endpoint
http://192.168.1.10:1234/v1/messages model=qwen3.8-flash-next`. That model is not
loaded on the server, so the chat then showed `The API returned 400: … Failed to
load model` — a clean error path as a bonus. Set back to `qwen/qwen3.8-27b` the
same way.

No key was stored on this device, so the Keystore round-trip is still only
verified on the emulator.

## 7. Copy — PASSED 2026-09-17

Clipboard goes through `ClipboardManager` on the UI thread, polled from Rust —
a path with no desktop equivalent.

**Steps.** Copy from: an assistant message, a code block inside one, a tool card,
and an error. Paste each into another app.

**Pass.** The pasted text matches (the tool card pastes as `$ command` then its
output). The chat status line briefly reads "Copied"; on Android 13+ the system
shows its own confirmation, below that a toast.

**Result.** Copy on a tool card, pasted into the message box with Ctrl-V, came
back as `$ uname -a\nLinux localhost 6.1.128-… aarch64 Linux\n` — exactly the
card, command and output together.

## 8. Terminal with a real shell — PASSED 2026-09-17

**Steps.** Terminal tab: `ls -la /`, `apk --version`, `git --version`, then an
interactive one (`vi`, `:q`). Use the extra-keys row for Esc and Ctrl.

**Pass.** Commands run (see check 1 for why this is phone-only), the screen
redraws correctly on rotation and when the keyboard opens, and Ctrl-C
interrupts.

**Result.** `git --version` → 2.47.3, `apk --version` → apk-tools 2.14.6 for
aarch64, and `yes hello | head -3` printed three lines and returned to the
prompt, which is the PTY half of the check-3 fix. `sleep 30` interrupted with the
Ctrl-C key: `^C`, prompt back, next command ran.

## 9. Git is present on a fresh install — PASSED 2026-09-17

The bootstrap installs git into a newly created rootfs, after writing
`/etc/resolv.conf` — an ordering that was wrong once and is invisible on an
already-provisioned device.

**Steps.**

```sh
adb uninstall net.pedrosoares.mobilecoder
apps/android/run-app.sh --logs
```

**Pass.** The log shows the rootfs install, then DNS, then `apk add git`
succeeding. `git --version` works in the Terminal tab on first launch.

**Result.** Run without uninstalling, by moving the rootfs aside
(`run-as … mv files/rootfs files/rootfs.keep`) so the existing toolchain was not
thrown away; restored afterwards. The log read: `rootfs downloaded (3850805
bytes)` → `verifying checksum` → `rootfs ready` → `guest resolv.conf updated` →
`installing git into the new guest` → `git installed` → `sandbox ready`, about
four seconds end to end. In the Terminal: `git clone --depth 1
https://github.com/octocat/Hello-World /tmp/hw` cloned over HTTPS and listed
`README`.

## 10. Fold-specific layout — NOT RUN (needs the phone unfolded)

**Steps.** Open and close the fold; rotate; open the keyboard on each screen.

**Pass.** The app bar clears the status bar and the hinge, the native message box
sits above the navigation bar, and the transcript resizes rather than sliding
under the keyboard.

**Result.** Only the cover display was tested, since folding is a physical act.
There, landscape works: forced to rotation 1, the app bar still cleared the
status bar, the transcript reflowed to the wider measure, and the message box
stayed above the gesture bar. The inner display and the fold transition are
still open.

## 11. A real build, end to end — PASSED 2026-09-17

The acceptance test for the whole idea: a program written, compiled and run on
the phone, with no computer involved.

**Steps.** Chat: `write a C program that prints the first 20 primes, compile it
with gcc and run it`. (`apk add build-base` may be part of the turn; it needs
`--link2symlink`, which the sandbox already passes.)

**Pass.** The program compiles and prints the primes. Keep the transcript.

**Result.** It did, and the interesting part is what went wrong first:

```
gcc: fatal error: cannot execute 'as': posix_spawnp: No such file or directory
```

`/usr/bin/as` and `/usr/bin/ld` listed and stat-ed fine but could not be opened —
leftovers of the hard-link handling in an earlier `apk add build-base` under
`--link2symlink`. The agent diagnosed that itself, reinstalled
(`apk del build-base binutils && apk add build-base binutils`, 242 MiB) and then:

```
$ gcc -O2 -Wall -o /tmp/primes /tmp/primes.c
$ /tmp/primes
2 3 5 7 11 13 17 19 23 29 31 37 41 43 47 53 59 61 67 71
```

A program written, compiled and run on the phone, by a model running on the
local network, with no computer in the loop.

**Worth following up:** whether a *fresh* `apk add build-base` produces those
unopenable entries every time, or whether this rootfs was left that way by an
earlier experiment. If it is reproducible, the bootstrap should install
build-base the way it installs git, or repair it.

## 12. Background jobs — PASSED 2026-09-18 (one assumption disproved)

Added after the list above, when the agent gained `run_in_background`,
`job_output` and `job_kill`. Verified end to end on the desktop against the same
local model — started a job, listed it, read it, killed it, confirmed the log
stopped growing — and by unit tests, including that a kill reaches a job's
grandchildren. What the phone still has to answer:

**Steps.**

1. Chat: `start a background job that appends the date to /tmp/tick.log every
   second, then tell me what jobs are running`.
2. Watch the chat's status line.
3. Chat: `read the job's output` (it writes to a file, so expect "nothing new").
   Then `run wc -l /tmp/tick.log twice, a few seconds apart`.
4. Switch tabs, send another message, then `kill the job` and check the file
   stopped growing.

**Pass.** The job survives other turns and tab switches, the status line reads
`Ready · 1 background job` while it runs and drops the suffix when it is killed,
and `job_kill` ends it — with no `sh`/`sleep` left behind
(`adb shell "ps -A -o PID,ARGS | grep tick.log"`).

**Also test the part only Android has.** Start a job, then:

```sh
adb shell am force-stop net.pedrosoares.mobilecoder
adb shell "ps -A -o PID,ARGS | grep -c 'tick.log'"      # expect a survivor
adb shell am start -n net.pedrosoares.mobilecoder/.MainActivity
adb logcat -d | grep "left by a previous run"
adb shell "ps -A -o PID,ARGS | grep 'tick.log'"          # expect none
```

**Pass.** The job does outlive the app — that is the platform, not a bug — and
the next launch kills it, logging how many it found. This is the one check that
cannot be done anywhere but a device, because it depends on Android killing the
app outright.

**Watch for.** A job started under proot means proot plus its tracees; the kill
must take the whole group, not just proot (the failure mode from check 2).

**Result (2026-09-18, `192.168.1.93:32875`).** The job half passed as written:

- `bash run_in_background` returned `job-1` and the turn ended while the job
  kept running — a separate foreground call a few seconds later showed
  `/tmp/tick.log` at 6 lines and climbing.
- It survived Chat → Terminal → Files → Chat, and survived two more turns: at
  109 seconds old it was still `running`, with 108 lines written, one a second.
- `job_output` said "(nothing new since the last read)" throughout, correctly —
  the loop writes to a file, so the job's own output really is empty.
- The status line read `Ready · 1 background job` the whole time, and dropped
  the suffix the moment the job was killed.
- `job_kill` ended it: the file stopped growing (164 lines before and after a
  3-second wait) and nothing was left in `ps` — proot, the shell and the sleep
  all gone, which is the process-group kill doing its job. `job_kill all` behaved
  the same.

**The Android half disproved its own premise.** The check assumed a job would
outlive the app and need cleaning up at the next launch. It does not:

| What killed the app | What happened to the job |
|---|---|
| `am force-stop` | gone with it |
| `kill -9 <app pid>` (the shape of a low-memory kill) | gone with it |

Android tears down the app's process group when the app process goes. And the
cleanup that had been written for this — a `/proc` sweep at startup for anything
running out of the rootfs — turned out to be impossible for an app anyway: it
reported `killed 0 of 0 match(es), out of 1 visible processes`. `/proc` is
mounted with `hidepid`, so the app sees only itself. A `run-as` shell sees over a
thousand, which is exactly what made this look like it would work when tried
from a terminal: `run-as` keeps the shell's `AID_READPROC` group, and the app has
no such group.

The sweep was removed rather than left as dead code, and the finding is in the
`mc_sandbox::jobs` module docs, where the next person to think of it will look.

## 13. Auto-scroll — verified on the emulator, still open on the phone

The chat follows a streaming reply, and stops following the moment the reader
scrolls up. Verified on the emulator, where a model reply streams exactly as it
does on the phone (no guest commands needed): the view stayed pinned to the
newest text through a fifteen-paragraph reply; scrolling up mid-stream left the
view where it was for the rest of that reply *and* the whole of the next one;
scrolling back to the bottom resumed following. It also survived the keyboard
opening, which shrinks the viewport.

What only the phone can answer is touch: the emulator was driven with synthetic
swipes, which are not the same as a thumb with momentum.

**Steps.** Ask for something long. While it streams: watch it follow; flick up a
short way and check it stays; flick back down to the bottom and check it
resumes. Then send another message from a scrolled-up position — sending is
meant to jump to the bottom, whatever the reader was looking at.

**Pass.** No jumping while reading, no lagging behind while watching, and no
stutter (the follow only ever scrolls downwards, so it cannot fight the
momentum of a flick).

## 14. Context compaction — verified live and on the emulator, open on the phone

The mechanics are covered by tests against a scripted server (refused → summarized → retried), and
the round trip was verified against a real model: with the threshold forced to one token, the first
exchange was summarized away, the passphrase it contained survived only in the summary, and the
model answered from it correctly (`cargo test -p mc-agent live_compaction -- --ignored`). The
emulator showed the meter on device: `Ready · 11k context`.

What the phone adds is a long real session rather than a forced one.

**Steps.** Work for a while — several file reads, a build, a few questions — watching the context
figure climb in the status line. Then either wait for it to pass the threshold, or point the app at
a small-context local model (16k, say) so the *refusal* path runs instead.

**Pass.** When it compacts: a notice appears in the transcript, the figure drops sharply on the next
turn, and the conversation carries on — ask about something from before the compaction and the
answer should come from the summary rather than a blank. Force-stop and relaunch: the first line
says earlier messages were summarized, and the session still works.

**Watch for.** A turn that fails with a 400 and *stays* failed is the bug this exists to prevent —
the recovery should compact and retry once, and a second failure should say something a person can
act on.

## 15. GitHub — verified on 2026-09-19 with a real token, up to the push itself

Built and checked as far as it can be without credentials. On the emulator: the Git tab renders, the
native token dialog opens, a typed token reaches the worker over JNI, GitHub answers, and a rejected
token is reported (`GitHub rejected the token: Bad credentials. Add a new one.`) *and* deleted from
the device (`github token deleted from this device` in the log). The transport is proven separately
against a real repository — `cargo test -p mc-github clone_through_the_proxy -- --ignored` clones
octocat/Hello-World through the loopback proxy and asserts `.git/config` holds the real HTTPS URL
with no trace of the proxy in it.

What only the phone can answer: everything past a *valid* token, since the emulator cannot run guest
commands at all (check 1).

**Steps.** Make a fine-grained token on github.com with *Contents: read and write* for one throwaway
repository. Git tab → **Add token** → paste. Then:

1. The account line should show your login, and the repository list should fill.
2. **Clone** one. It lands in `/root/projects/<name>` and appears under Projects, selected, with a
   status line like `main · clean`.
3. Edit something (Terminal: `echo hi >> README.md`), come back — status should say `1 change` —
   then **Commit** with a message and **Push**.
4. Check on github.com that the commit is there, then **Pull** (should say up to date).
5. **New repository**: a name, then it is created private with a first commit and cloned.
6. **Sign out**, then reopen the app: it should be signed out and the sealed token gone.

**Pass.** Each action ends with a sentence in the footer rather than a spinner that stops. After a
push, `git -C /root/projects/<name> config --get remote.origin.url` is the **github.com** URL — if
`127.0.0.1` appears there, the proxy leaked into the repository and that is a bug.

**Watch for.** The token must appear nowhere: not in `adb logcat` except as `github_p… N chars`, not
in `.git/config`, not in any command line (`ps -A -o ARGS | grep -i token` during a push).

## 16. Multiple chats — verified on the emulator, quick to confirm on the phone

Chats are separate conversations, listed under the title bar in the Chat tab. Verified on the
emulator against a real model, since none of it needs guest commands: a new chat opened empty, took
its own turn, and was titled by its first message; the picker listed all three with message counts,
newest first; switching back brought a 23-message transcript back intact; deleting removed the file
and the row; and after a force-stop the chat that reopened was the one last *opened*, not the one
last left. The single-session file from before chats were plural was adopted on first launch rather
than orphaned.

Two defects found and fixed there, both worth re-checking with a thumb rather than `adb input tap`:
the title's tap target hugged its text, so tapping past a short title did nothing; and the list did
not redraw after a delete.

**Steps.** Chat tab → **New**, ask something, then open the picker, switch back to the old chat, and
switch forward again. Delete one. Force-stop and relaunch.

**Pass.** Each switch brings the right transcript, the title bar matches it, and nothing from one
chat appears in another. The context figure in the status line should also change with the chat —
it is per-conversation.

## 17. Nothing freezes when the app leaves the screen

The bug this fixes, in the reporter's words: "if a http server is running the browser is stuck
waiting to the app be on foreground again to load the page." Measured on the emulator before the fix
— a streaming turn died 13 s after the home button and could not reconnect — and again after it,
where a 150-paragraph reply streamed to completion entirely in the background with no stream breaks.

The emulator cannot host the real case, since it cannot run guest commands at all (check 1).

**Steps.**

1. Terminal tab: `python3 -m http.server 8080 &` (`apk add python3` first if needed), or any server.
2. Check the notification appears saying **Serving**.
3. From a browser on the phone, load `http://127.0.0.1:8080/`. Then press home, wait a minute with
   the app off screen, and load it again. From another machine on the same network it is
   `http://192.168.1.93:8080/`, which is the better test — it needs no app switching at all.
4. Kill the server. The notification should go within a second or two.
5. Repeat with the switch in ⚙ instead (`Keep the sandbox running in the background`) and a
   *non*-listening job, say `while true; do date >> /tmp/tick.log; sleep 1; done &` — the file should
   keep growing while the app is in the background.
6. Press **Stop** on the notification: the service goes, the switch in ⚙ is now off, and it does not
   come back by itself.

**Pass.** Pages load with the app in the background. The notification tracks what is actually
running, and disappears when nothing is.

**Watch for.** The notification must *not* linger with nothing running — that is a foreground
service holding the phone awake for no reason, which on a battery is the worst version of this bug.
Also worth a look: `adb shell dumpsys activity services net.pedrosoares.mobilecoder | grep
isForeground` should show nothing when idle.

**Still open:** Doze. A foreground service keeps the app out of the *freezer*, but with the screen
off for a long time the system can still restrict network access for an app that is not exempt from
battery optimisation. If a server turns out to become unreachable after a long screen-off period,
that is the next thing to measure, and the fix is an exemption the user grants.


### Check 15, run for real (2026-09-19)

Signing in, the repository list, project status, clone and the proxy transport all work on the
phone with a real fine-grained token: the pane read `Signed in as pedrosoares`, listed the account's
repositories, and a push reached GitHub through the loopback proxy and came back with GitHub's own
answer.

Three bugs found, all fixed:

1. **A repository committed inside the sandbox was corrupt.** proot's hard-link emulation broke how
   git writes objects, so `refs/heads/main` pointed at a commit that could not be read
   (`fatal: bad object HEAD`) and push aborted at the status read that precedes it. Fixed with
   `core.createObject=rename`, set globally in the guest at every launch and on every git command
   the app runs. See `docs/ARCHITECTURE.md` §9.6.
2. **Failures were invisible.** `mc-github` and `mc-ui` log through `tracing`, which on Android went
   nowhere - the app bridges the `log` crate to logcat. Enabling `tracing`'s `log` feature put them
   on that bridge, and `git()` now logs the command and git's own words when a command fails.
3. **The result never reached the screen.** The project card was pressable as a whole, so pressing
   Push *also* re-selected the project, and the refresh that followed replaced the push's outcome
   with nothing - "no success and no error", exactly as reported. Only the name and status line are
   pressable now.

What remains is a token question, not a code one: the push returned
`Permission to pedrosoares/mobile-coder-page.git denied to pedrosoares` (403), which is a
fine-grained token without *Contents: Read and write* for that repository. The pane now says that in
those words rather than quoting git at the user.

**Still to verify:** a push that actually succeeds, and `Pull` - both need a token with write access.
