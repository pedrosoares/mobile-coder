# mobile-coder — Architecture

A coding-agent workstation that runs entirely on an Android phone: a Rust core, a
[Freya](https://freyaui.dev) UI, a Claude-driven agent loop, and a proot/Alpine userland that
gives the agent a real Linux toolchain to build against. No desktop, no server.

Status: design document. No code yet.

---

## 1. The shape of the system

Three layers, each a hard boundary:

| Layer | Owns | Never does |
|---|---|---|
| **Shell** (Freya UI) | Rendering, input, session/project navigation | Talk to Claude; touch the rootfs |
| **Agent** (Claude loop) | Conversation state, tool dispatch, streaming | Render; execute anything itself |
| **Sandbox** (proot + Alpine) | Process execution, filesystem, toolchains | Know Claude exists |

The agent's *only* effectors are sandbox operations. That keeps the dangerous surface small
and auditable: every side effect the model causes is one of a handful of typed calls into
`mc-sandbox`.

---

## 2. Findings that constrain the design

These were verified against upstream sources, not assumed. They are the reason the design
looks the way it does.

### 2.1 Freya's Android support is real, but only on the 0.5 pre-release

`freya-android` exists as a first-class workspace crate at **0.5.0-rc.6**, built on `jni 0.22`.
It ships an `AndroidPlugin` that provides the `AndroidApp` handle as root context and a root
component that syncs the status bar to the theme and shows/hides the soft keyboard based on
whether the focused node has an IME accessibility role.

Consequences:

- **Target `0.5.0-rc`, not `0.4.3`.** Stable has no Android story at all. Accept pre-release churn.
- The upstream `examples/android/README.md` warns that "soft keyboard and IME support are not
  yet implemented." **That README is stale** — `crates/freya-android/src/keyboard.rs` and the
  IME-role side effect are on `main`. Verify against the crate, not the README.
- Upstream already has **`freya-terminal`** (a real VT on `rio-vt` + `portable-pty`) and
  **`freya-code-editor`**. Do not write our own editor buffer.
- **Correction: on-screen keyboard text does not reach Freya's `Input` on Android.** This
  document previously called the upstream README's IME warning stale, because
  `freya-android` shows and hides the keyboard. Showing it works; *receiving text from it* does
  not. Measured on the API 36 emulator on 2026-09-16: with the chat input focused and the
  keyboard open, tapping keys on the on-screen keyboard changed nothing, and neither did its
  enter key. `adb shell input text` does work, which is misleading - it injects raw key events
  and bypasses the IME entirely. The likely cause is structural: Freya runs in a
  `NativeActivity`, which exposes no `InputConnection`, the channel keyboards commit text
  through. `GameActivity` exists largely to fix this. Not yet confirmed on a physical keyboard
  (Samsung's), but it uses the same channel.

  **Resolution: the Android message box is a native `EditText`** in a `PopupWindow` over the
  Freya surface (`NativeComposer.kt`); `mc-ui` hides its own composer when
  `MobileCoder::native_composer` is set. Two things had to be learned to get there:
  - Views added with `addContentView` are useless in a `NativeActivity`. It takes over the
    window's surface and input queue, so the views exist in the hierarchy (uiautomator lists
    them) but are never drawn and never touched. A `PopupWindow` is a separate window with its
    own surface and input - which is also what lets the IME attach to it.
  - A focusable popup is modal by default; `isTouchModal = false` lets touches reach the chat.
    The keyboard inset must be read from the *popup's* window, which is the IME target.

  Verified on the emulator by tapping on-screen keyboard keys: text arrives with IME behaviour
  intact (auto-capitalisation, suggestions, voice key), the bar tracks the keyboard, Send is
  disabled while a turn runs. The cost: the message box is Android-styled rather than a Freya
  component, and desktop keeps Freya's `Input`.
- **Two more Freya 0.5.0-rc.6 gaps, found building the chat's markdown:**
  - The `markdown` feature cannot be enabled: `freya` requires `freya-markdown ^0.5.0-rc.6`,
    and the newest on crates.io is `rc.3`. `mc-ui/src/markdown.rs` renders replies instead,
    via `pulldown-cmark`.
  - A `Span`'s own `font_family` is ignored - `Span::to_text_style` takes the font list from
    the parent paragraph's state rather than the merged span style. So monospace works for a
    whole paragraph (code blocks, tool output) but not for inline code inside a sentence,
    which is marked by colour only. Also, on Android no system font resolves by name
    (`monospace`, `Droid Sans Mono`); the shell reads `/system/fonts/DroidSansMono.ttf` and
    registers it with `LaunchConfig::with_font`.
- **`freya-terminal` did not cross-compile to Android; it does now, with three small patches**
  vendored under `patches/` and wired in with `[patch.crates-io]`:
  - `teletypewriter` declared its `termios` only for linux/macos/freebsd. *Correction:* an earlier
    version of this document said bionic's `termios` matches glibc's, so the Linux branch would
    nearly compile as-is. It does not - bionic's struct lacks glibc's `c_ispeed`/`c_ospeed` - so
    Android got its own branch. `TIOCSWINSZ` needed an Android definition too, and
    `#[link(name = "util")]` had to be skipped: bionic has `openpty`/`forkpty` in libc and ships
    no libutil.
  - `rio-vt`'s kitty-graphics shared-memory transport calls `shm_open`, which bionic lacks. On
    Android that one medium reports itself unavailable; a proot guest could not share memory
    with the app anyway.
  - The stack includes C++ (`simdutf`), which links `libc++_shared`. The APK must carry it, or
    the app dies at load with *library "libc++_shared.so" not found* - hence
    `--link-libcxx-shared` in the Gradle task.

  The Terminal pane runs a login shell under the same proot setup as the agent's tool calls.
  Input on Android comes from the native message box in a terminal mode (a line plus Enter, and
  Esc/Tab/Ctrl-C/arrow keys), since Freya receives no on-screen keyboard text there.

  **Pane state lives in the app root, not in the panes.** A pane is unmounted when its tab is
  left, taking its state *and its spawned tasks* with it - so the chat used to lose its history
  and, worse, stop recording: a reply arriving while the user was on another tab was gone for
  good. The chat log, the composer draft, the shell session and the Files directory are all owned
  by `MobileCoder`, and the two long-lived tasks (folding agent events into the log, delivering
  Android keyboard input to the shell) use `spawn_forever`, which attaches to the root scope.
  A running shell therefore survives tab switches and ends with the app; measured on the Fold6:
  the same proot pid before and after switching away, and zero proot processes after force-stop.

  This matters more than a missing pane: a terminal is central to a coding workstation, and the
  plan leaned on `freya-terminal` precisely to avoid writing a VT. Routes out, in order of
  preference: **(1)** upstream an Android branch in `teletypewriter` — small, since bionic's
  `termios` has `c_ispeed`/`c_ospeed` just like glibc, so the existing Linux arm very nearly
  compiles as-is; **(2)** carry a `[patch.crates-io]` fork until that lands; **(3)** render
  `mc-pty` output ourselves, which is the scope we were trying to avoid.

The upstream Android example is also the build template: a `cdylib` + `rlib` crate exposing
`#[unsafe(no_mangle)] fn android_main(AndroidApp)`, driven by a Gradle project in `AndroidApp/`,
compiled with `cargo-ndk` against **NDK r26d** for `aarch64-linux-android`.

### 2.2 The execution restriction is the whole ballgame

This is the load-bearing constraint of the entire project, and it is easy to get wrong.

On Android 10+, an app whose **`targetSdkVersion` is 29 or higher** lands in an SELinux domain
that forbids `exec()` on anything under `/data/data/<pkg>`. Google's framing: *"Execution of
files from the writable app home directory is a W^X violation. Apps should load only the binary
code that's embedded within an app's APK file."* This is why Termux still ships with
`targetSdkVersion 28`.

**proot works — and the reasoning that said it would not was wrong.** This section originally
argued that proot could not help, because proot translates *paths* while W^X is an *execute
permission* enforced by the kernel, so the kernel would still refuse the guest `execve`. The
first half is right and the conclusion is not. Measured on 2026-09-15 (`EXEC-PROBE.md`): at
`targetSdk 36` with SELinux enforcing, direct `execve` from app data is denied with `EACCES`
**and** proot runs a guest binary from a rootfs in app data successfully.

The reconciliation is the real finding: **proot never asks the kernel to `execve` the guest.**
It ptraces the tracee and hands control to its own small loader ELF — which ships in the native
library directory, always executable — and that loader **maps** the guest binary itself. Mapping
executable pages out of app data is evidently still permitted; only `execve` is denied. proot
sidesteps the rule rather than defeating it.

Keep the distinction between path translation and execute permission, because it is real and
widely muddled. Just do not conclude from it, as this document previously did, that proot cannot
carry a Linux userland. It can.

There are exactly two real escapes:

**(a) `targetSdkVersion 28`.** The app gets the legacy SELinux domain, and exec from app data
works normally. This is what Termux does and what `proot-distro` relies on.
*Cost:* Google Play requires new apps and updates to target API 36 from 31 Aug 2026, so this is
**sideload / F-Droid distribution only, permanently.** `targetSdk 28` is far above the ~SDK 23
floor Android 14+ enforces on installs, so it remains installable.

**(b) `system_linker_exec`.** Instead of executing `/path/to/prog`, execute
`/system/bin/linker64 /absolute/path/to/prog` — the kernel only ever sees the system linker
being executed, which is allowed. This is what `termux-exec` does via an `LD_PRELOAD` shim.
*Costs:* statically linked binaries do not work at all; `/proc/self/exe` reports the linker
rather than the real program; and the `LD_PRELOAD` interception only catches programs that go
through libc wrappers, not ones issuing `execve` as a raw syscall.

**Escape (b) is closed to every Linux distribution, not just Alpine.** This is worth stating
plainly because it is easy to misread as a musl problem that a distro swap would fix.
`/system/bin/linker64` is *bionic's* loader. It can load bionic-linked executables. Alpine
binaries declare `/lib/ld-musl-aarch64.so.1` and link against musl; Debian and Ubuntu binaries
declare `/lib/ld-linux-aarch64.so.1` and link against glibc. **Both are equally foreign to
bionic** — different libc ABI, different TLS layout, and in glibc's case symbol versioning that
bionic does not implement. `termux-exec` works precisely because Termux's own packages are
compiled against bionic, which is what makes Termux not a distro in the proot sense.

There is no Linux distribution that is bionic-native, so there is no distro choice that reopens
escape (b). The only thing that would is abandoning the idea of a Linux userland altogether and
shipping bionic-compiled tools the way Termux does.

**Decision: `targetSdkVersion 36`. Measured, not assumed — and it works.**

Escape (a) is the safe option and this doc originally recommended it. It is not the one chosen,
because giving up Google Play permanently is a large, irreversible price to pay for a claim
nobody here has actually tested. So the claim gets tested.

`tools/exec-probe` settles it on real hardware — see `EXEC-PROBE.md`. It is a real APK at
`targetSdk 36`, because the check only means something from inside the `untrusted_app` SELinux
domain: `adb shell` runs as a different user where `/data/local/tmp` *is* executable, so a
shell-based test passes and proves nothing.

It has now been run. The control held (direct exec blocked, `EACCES`, context
`u:r:untrusted_app:s0`), and `proot-guest` returned `alive-inside-proot`. **Escape (a) is not
needed and Google Play distribution stays open.**

One negative worth keeping: `linker-musl` failed with *COPY relocations are not supported*.
bionic's linker did locate musl's libc when pointed at the rootfs, then hit a genuine ABI
incompatibility. So the `system_linker_exec` route really is closed to a foreign-libc userland —
it is simply not the route proot uses, which is why the end-to-end result is unaffected.

Re-run on a physical arm64 device before shipping: the mechanism is architecture-independent,
but OEM SELinux policies vary.

> If Play distribution ever becomes non-negotiable, the escape is not a cleverer proot; it is
> abandoning Alpine and shipping a **bionic-native** toolchain the way Termux does, at which
> point `system_linker_exec` works and `targetSdk` can be current. That is a different project.

### 2.3 Rust has no official Anthropic SDK

There is no first-party Anthropic crate. `mc-agent` therefore speaks the Messages API over raw
HTTP — `reqwest` plus an SSE parser. This is a small amount of code, but it means we own the
wire format: streaming event assembly, `tool_use` block accumulation, and error taxonomy are all
ours to get right. Budget for it explicitly rather than treating the API client as a one-liner.

### 2.4 HTTPS on Android needs an explicit root store

`reqwest`'s rustls backend, given no explicit roots, reaches for
`rustls-platform-verifier` — which on Android must be handed a JNI environment and an Android
`Context` before first use. Miss that and the first HTTPS request panics with *"Expect
rustls-platform-verifier to be initialized"*, on a worker thread, long after startup looked
healthy. It cost a debugging cycle here.

Initialising it properly is awkward in this tree: `rustls-platform-verifier` builds against
`jni` 0.21 while `freya-android` uses `jni` 0.22, so both versions would have to be threaded
through. `mc_core::http` instead builds every client with an explicit rustls config over the
bundled Mozilla roots — no JNI, no `Context`, and identical behaviour on desktop and device,
which makes TLS failures reproducible off-device.

The tradeoff is real: user-installed and enterprise CAs are not trusted. For `api.anthropic.com`
and a distro mirror that is right; if the app ever needs to sit behind a TLS-inspecting
corporate proxy, this is the thing to revisit.

### 2.5 Choose the distro on toolchain grounds, not Android ones

Since §2.2 is distro-independent, the distro decision is free of the Android constraint and
should be made on what actually makes a coding workstation pleasant.

**Prefer a glibc distro — Debian or Ubuntu — over Alpine.** Not because of anything above, but
because an agent on this device will spend its life installing toolchains, and the prebuilt
binaries the world ships are overwhelmingly built against glibc. `rustup`, Node distributions,
Go toolchains, prebuilt language servers and vendored CLI binaries all assume glibc by default;
on musl they variously need a source build, a distro-specific package, or simply fail with a
confusing loader error. Alpine is an excellent container base for exactly the reason it is a
mediocre developer workstation: it is small because it is not glibc.

Debian bookworm is the safe default. Alpine remains a legitimate choice if image size dominates
and the toolchain set is fixed and known-good, but for an agent that will be told "install X and
build it", glibc removes a whole category of failure the model would otherwise have to debug.

The probe deliberately tests **Alpine/musl** rather than Debian, because it is the harder case
for bionic's linker. If musl passes, glibc is not thereby proven - but if musl fails, that is the
strongest available signal that the whole foreign-libc approach is closed at this `targetSdk`.

---

## 3. Crate layout

A Cargo workspace. The split exists so the agent and sandbox are testable on the desktop,
where the dev loop is measured in seconds rather than in APK installs.

```
mobile-coder/
├── Cargo.toml                  # workspace
├── crates/
│   ├── mc-core/                # session & project model, config, event bus
│   ├── mc-sandbox/             # proot lifecycle, Alpine rootfs, process spawn
│   ├── mc-pty/                 # PTY allocation + I/O pump
│   ├── mc-agent/               # Claude Messages API loop, tool dispatch
│   └── mc-ui/                  # Freya components (shared desktop/Android)
└── apps/
    ├── desktop/                # bin — the fast dev loop
    └── android/                # cdylib + AndroidApp/ Gradle project
```

**Why desktop and Android share everything above `apps/`:** the upstream Freya example compiles
as both a `cdylib` (for `android_main`) and a plain binary. We keep that property. Nearly all
development happens against `apps/desktop`; the Android target is for integration, not for
iteration. The one layer that genuinely differs is `mc-sandbox`, which needs a desktop backend
(plain `std::process`, or a local container) standing in for proot.

### Crate responsibilities

**`mc-core`** — The session model: a project is a directory in the rootfs; a session is a
conversation plus its transcript plus its working directory. Owns persistence and the event bus
that carries streaming deltas to the UI. Depends on nothing else in the workspace.

**`mc-sandbox`** — Owns the Alpine userland end to end: first-run rootfs extraction, proot
invocation, environment construction, process spawn and reaping. Exposes a narrow trait so
`mc-agent` and the desktop backend can be swapped without the agent noticing.

- The `proot` binary itself is built for `aarch64-linux-android` (bionic) and shipped as
  `libproot.so` in `jniLibs`, so it lives in the APK's native library directory and is
  executable unconditionally — independent of the `targetSdk` question in §2.2.
- The rootfs should be **acquired on first run**, not shipped in the APK. Modern `proot-distro`
  assembles containers from **OCI images** (`debian:bookworm`, `ubuntu:24.04`, `alpine:3.21`),
  which is the model to copy: it gives us pinned, checksummed, widely mirrored rootfs images for
  free instead of a bespoke tarball we host ourselves. Extraction into
  `/data/data/<pkg>/files/rootfs` is a one-time cost that needs a visible progress UI and must be
  resumable.

**`mc-pty`** — Wraps `portable-pty`. Lives apart from `mc-sandbox` because the agent's
non-interactive tool calls want captured stdout/stderr and an exit code, whereas the user's
interactive terminal wants a live PTY; conflating them produces a bad version of both.
*To verify:* bionic exposes `openpty`/`forkpty` and `/dev/ptmx` is reachable from an app
sandbox — Termux depends on this, but confirm it directly on device early.

**`mc-agent`** — The loop. See §4.

**`mc-ui`** — Freya components. Composes `freya-terminal` and `freya-code-editor` rather than
reimplementing them. Phone-first layout: a single-pane view with a switcher, not a desktop IDE's
split panes scaled down.

---

## 4. The agent loop

`POST /v1/messages`, streaming, with a client-side tool-execution loop.

**Model:** `claude-opus-5`.

**Request shape:**

- `stream: true` always. Long tool-use turns will otherwise hit HTTP timeouts.
- `thinking: {type: "adaptive"}` — the fixed `budget_tokens` form is rejected with a 400 on
  Opus 5. Set `display: "summarized"` if we want to show reasoning in the UI; the default is
  `"omitted"`, which on a phone reads as a long dead pause before any output appears.
- `output_config: {effort: ...}`. This is the main cost/quality dial and it belongs in user
  settings — on a phone, on cellular data, on a battery, the user has a legitimate interest in
  turning it down. `xhigh` suits coding work; `low` is right for trivial edits.
- `max_tokens` around 64000 for streaming turns.

**The loop:** send → stream deltas to the UI over the `mc-core` event bus → on
`stop_reason == "tool_use"`, execute every `tool_use` block, then send **all** `tool_result`
blocks back in a **single** user message. Splitting them across messages teaches the model to
stop making parallel calls. A failed tool returns `tool_result` with `is_error: true` — never a
dropped block.

**Prompt caching** matters more here than on a desktop, because it cuts both cost and latency on
a slow link. Render order is `tools` → `system` → `messages`; any byte change invalidates
everything after it. So: a frozen system prompt, a deterministically ordered tool list, and
nothing volatile (timestamps, session IDs) in the prefix. Verify with
`usage.cache_read_input_tokens` — a persistent zero means something upstream is churning.

**Long sessions** should use server-side compaction rather than a hand-rolled truncator. The
critical detail: append the whole `response.content` back into the message history, not just the
extracted text. The compaction blocks are load-bearing state, and stripping them to a string
silently loses it.

**Refusal handling:** check `stop_reason` before reading `content`; `"refusal"` arrives as an
HTTP 200. Enable server-side fallbacks so a refusal reroutes instead of dead-ending the session.

### Tool surface

Start deliberately small. Every tool is a typed call into `mc-sandbox`:

| Tool | Notes |
|---|---|
| `bash` | Runs inside proot/Alpine, captured output, timeout, cwd-scoped |
| `read_file` | Offset/limit, so a large file can't blow the context |
| `write_file` | Whole-file write |
| `edit_file` | Exact string replacement — cheaper and safer than rewrites |
| `glob` / `grep` | Search without spending a `bash` turn |

`bash` alone would technically be sufficient, and it is tempting on a small screen. Resist it:
dedicated file tools produce structured, reviewable diffs, which is what makes an
approval UI possible at all on a phone.

---

## 5. Flow of a single turn

```
User types in mc-ui
      │
      ▼
mc-core  ── session state, transcript ──┐
      │                                 │
      ▼                                 │
mc-agent ── POST /v1/messages (SSE) ──▶ Claude API
      │                                 │
      │◀──── deltas ────────────────────┘
      │
      ├── text deltas ──▶ event bus ──▶ mc-ui (streams into view)
      │
      └── stop_reason == tool_use
                │
                ▼
          mc-sandbox ──▶ proot ──▶ Alpine rootfs
                │              (cargo / gcc / git / …)
                ▼
          tool_result blocks ──▶ back into the loop
```

---

## 6. Build and distribution

**Toolchain:** Android SDK (API 36 for `compileSdk`), NDK **r26d**, `cargo-ndk`, and the
`aarch64-linux-android` Rust target. `x86_64-linux-android` is worth adding for emulator work.
None of this is installed on the current dev machine yet.

**`compileSdk 36`, `targetSdk 36`, `minSdk 24`.** Current target, Play distribution retained,
contingent on the probe. If the probe comes back negative, dropping to `targetSdk 28` is a
one-line change: `compileSdk` and `targetSdk` are independent knobs, and Termux ships exactly
that combination today (`compileSdkVersion=36`, `targetSdkVersion=28`, `minSdkVersion=21`).

**`ANDROID_HOME` is required even for `cargo check --target aarch64-linux-android`.** Not only
for linking, as one would expect: `mundy`, a transitive dependency of Freya, locates `android.jar`
in its build script and panics without it. Worth knowing before concluding the Android target is
broken.

**Distribution:** F-Droid or direct APK. Not Google Play — see §2.2.

**Dev loop:** `cargo run -p desktop` for essentially all work. Gradle `assembleDebug` +
`adb install` only for integration passes.

---

## 7. Risks and open questions

Ordered by how much damage each does if it goes the wrong way.

1. ~~**Does proot run guest binaries at `targetSdk 36`?**~~ **Answered 2026-09-15: yes.**
   Verified on an emulator at API 36 with SELinux enforcing. Remaining work: confirm on a
   physical arm64 device, since OEM SELinux policies vary.
2. **Thermal throttling and battery.** Now the largest genuine unknown. Sustained compilation on
   a phone throttles hard; nothing in the architecture fixes it.
3. **Freya 0.5 is a release candidate.** Android support arrived recently and the docs already
   lag the code. Expect breakage on upgrades; pin exact versions and read the source.
4. **Memory.** proot + Alpine + a Rust toolchain + a Skia-backed UI in one app process, against
   an Android low-memory killer that is not sympathetic. Measure early.
5. ~~**API key storage.**~~ Done: `KeyVault.kt` seals the key under the Android Keystore.
6. **Android 16's AVF-based Linux Terminal** may offer a real VM instead of proot on supported
   devices — *unverified*, but worth a look before committing hard to proot, since it would
   sidestep §2.2 entirely.
7. **Phone storage.** A few repositories with build artifacts (`target/`, `node_modules`) plus a
   rootfs with toolchains fill a phone quickly. §9 plans GitHub integration largely to answer this.
8. **A GitHub token inside the sandbox would be readable by the agent.** §9.2 keeps it out.

---

## 8. Suggested build order

1. **Spike the execution path on real hardware.** `targetSdk 28`, proot from `jniLibs`, a
   minimal rootfs, running `busybox sh`. Cheap, and it retires the project's biggest unknown
   before any UI exists.
2. **`mc-sandbox` + `mc-pty` on desktop**, behind the trait, with a `std::process` backend.
3. **`mc-agent` against the desktop sandbox.** Streaming, tool loop, caching. Fully testable
   with no phone involved.
4. **`mc-ui` on `apps/desktop`**, composing `freya-terminal` and `freya-code-editor`.
5. **Android integration:** the Gradle shell, `cargo-ndk`, rootfs download/extract UI, Keystore.
6. **Phone-shaped UX:** the part that actually decides whether this is usable — approval flows,
   one-handed navigation, keyboard handling.

Steps 2-4 need no Android toolchain at all, which is why the crate boundaries are drawn where
they are.

---

## 9. GitHub integration (planned)

**Goal:** clone, commit, push and pull from the phone, with GitHub as the place code lives.

**Why it matters for storage:** a phone should hold working copies of what is being worked on
*now*, not an archive. With a reliable push, a finished project can be removed from the device and
cloned back when needed. Without it, every project is stuck using storage indefinitely.

Nothing in this section is built yet. The facts it relies on were checked on 2026-09-17.

### 9.1 What was checked

- **gitoxide (`gix`, pure Rust) cannot do this yet.** Its own `crate-status.md` lists `push`,
  send-pack client plumbing, `checkout`/`switch`/`reset` and partial clone all as unimplemented.
  A pure-Rust stack would be the simplest to cross-compile, so this is worth re-checking later.
- **libgit2 through `git2` 0.21 (libgit2 1.9.7) can.** It covers clone, fetch, commit and push,
  plus shallow fetch via `FetchOptions::depth`. Local (`file://`) shallow clones are not supported,
  which only affects tests.
- **`git2`'s HTTPS goes through OpenSSL** (with a `vendored-openssl` feature). Cross-compiling
  OpenSSL for Android is **unverified**; see §9.6.
- **GitHub's OAuth device flow** needs only a `client_id` (no client secret, so it is safe to ship in
  an app). It must be enabled in the app's settings, and the user code expires after 15 minutes.

### 9.2 Decision: git runs in the app process, not inside the sandbox

Running `git` inside Alpine is the obvious choice and the wrong one. Everything in the guest shares
one sandbox with the agent's `bash` tool. Any token `git` can read there, a command the model runs
can read too, and a prompt injection in a cloned repository's files could send it out.

So user-facing git operations run **in the app process**, in a new `mc-git` crate on top of `git2`.
It works on the rootfs through the host path, using the same guest-to-host mapping as
`mc_sandbox::fs::RootedFs`. The token then lives only in app memory, never in a file or a process
environment the guest can see.

**How far that isolation goes is not yet proven.** A first draft of this section claimed Android's
Yama ptrace policy keeps guest processes out of the app's memory. Measured on the API 36 emulator,
that is wrong:
- the kernel has **no Yama** (`/proc/sys/kernel/yama/ptrace_scope` does not exist);
- a process with the app's uid **can read the app's `/proc/<pid>/environ` and `maps`**.

So: **never pass the token through an environment variable**, since that is readable. Whether
guest processes can read `/proc/<pid>/mem` depends on whether the app process is *dumpable*.
Debuggable builds are dumpable by design and release builds normally are not. Measuring this from
inside the guest, on a **release** build on the phone, is part of step 1 of the build order. The
decision stands either way, because the alternatives (a token in the guest filesystem, in
`.git/config`, or in the environment) are exposed for certain.

Two rules follow from this, and they are easy to break by accident:

- **Never put the token in a remote URL.** `https://x:TOKEN@github.com/...` gets written into
  `.git/config`, inside the rootfs, where the agent can read it. Provide the token only through
  `git2`'s credentials callback.
- **Never log it.** Redact it the way the API key is redacted (`sk-ant-a… 49 chars`).

`apk add git` inside the guest stays available for local, credential-free work: `status`, `diff`,
`log`, local commits.

### 9.3 Authentication

1. **First: a fine-grained personal access token.** The user creates it on github.com, limited to
   specific repositories with *Contents: read and write*. It is stored with `KeyVault` like the API
   key. This needs no infrastructure and gives the narrowest permissions.
2. **Later: the device flow** ("enter code ABCD-1234 at github.com/login/device"). This needs a
   registered GitHub App or OAuth App to supply the `client_id`. **Registering one is your decision
   and your account's action.** A GitHub App is the better fit: permissions per repository, and user
   tokens that expire and refresh.

### 9.4 Operations

| Operation | Behaviour |
|---|---|
| **Clone** | HTTPS, **shallow (depth 1) by default**, into `/root/projects/<name>`. Full history on request. |
| **Pull** | Fetch, then **fast-forward only**. If the branches diverged, stop and say so. Resolving merge conflicts on a phone is out of scope for v1. |
| **Commit** | Stage the changes shown in a reviewable list, with a message typed by the user or drafted by the agent. |
| **Push** | **Always an explicit user action**, confirming branch and remote. It publishes code, so the agent never pushes on its own. |

The agent gets local, credential-free tools: `git_status`, `git_diff`, `git_commit`. Anything that
talks to GitHub stays a user action.

### 9.5 Storage features

- **Shallow clones by default.** History is usually the largest part of `.git`.
- **Sizes per project**, split into working tree, `.git`, and build artifacts (`target/`,
  `node_modules/`, `.gradle/`). Build artifacts usually dwarf the source, so **"Clean build
  artifacts"** is the cheapest large saving.
- **Offload**: delete the local copy, keeping only the project's entry to clone it back later.
  This is the one feature here that can lose work, so it runs only when **all** of these hold:
  - no uncommitted changes;
  - no untracked files that are not ignored;
  - every local commit exists on the remote-tracking branch, checked **after a fresh fetch**;
  - no stashes.

  Otherwise it refuses and says which condition failed. It never "offloads anyway".
- **Rootfs housekeeping:** clearing the `apk` package cache.

Limits to be upfront about: libgit2 has no partial clone or sparse checkout, so a large monorepo is
still large, and Git LFS is not supported.

### 9.6 Build order

1. **Spike: `git2` on Android with HTTPS**, the same way `tools/exec-probe` settled proot. Build with
   `vendored-openssl`, then clone and push a throwaway repository from the phone. In the same spike,
   **measure token isolation on a release build**: from a guest process, try to read the app's
   `/proc/<pid>/mem`, and record the result in `EXEC-PROBE.md`. If OpenSSL will not
   cross-compile cleanly, fall back to **registering a custom smart-HTTP transport**
   (`git2::transport::register`) over the existing `reqwest` + rustls client. That avoids OpenSSL
   entirely and reuses the bundled root store from §2.4.
2. **`mc-git` on desktop:** clone, status, commit, fetch + fast-forward, push, and the offload
   safety checks, tested against local bare repositories. Shallow-clone tests need a network
   remote, since libgit2 does not do local shallow clones.
3. **Credentials:** token entry in the app, stored with `KeyVault`, used only in the credentials
   callback. Include a test that the token never appears in `.git/config` or the logs.
4. **Projects UI:** list each project with branch, ahead/behind, uncommitted changes and size,
   with Clone, Pull, Commit, Push, Clean and Offload actions.
5. **Agent git tools** (local only), plus Commit and Push buttons that act on what the agent
   proposes.
6. **Device flow**, once a GitHub App is registered.

### 9.7 Open decisions

- **Token first, or register a GitHub App now for the device flow?** The token is faster to ship;
  the App gives a better login and expiring credentials.
- **May the agent create local commits by itself,** or only propose them? Either way it never pushes.
- **If the release-build measurement shows app memory is readable from the guest,** decide whether
  the token is acceptable in-process anyway (scoped to a few repositories, short-lived), or needs
  stronger separation, such as git in a separate Android service process with its own lifecycle.

