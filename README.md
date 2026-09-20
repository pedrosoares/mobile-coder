# mobile-coder

A coding-agent workstation that runs entirely on an Android phone: a Rust core, a
[Freya](https://freyaui.dev) UI, a Claude-driven agent loop, and a proot Linux
userland that gives the agent a real toolchain to build against. No desktop, no
server.

Design: [`docs/ARCHITECTURE.md`](docs/ARCHITECTURE.md).

## Status

**It runs on Android.** The app installs an Alpine userland on first launch and
executes commands in it under proot, at `targetSdk 36` with SELinux enforcing:

```
[guest] uname -a               => Linux localhost 6.6.66-android15-8 ... x86_64 Linux
[guest] cat /etc/alpine-release => 3.21.7
[guest] id                      => uid=0(root) gid=0(root)
```

The foundational question was measured rather than assumed — see
[`docs/EXEC-PROBE.md`](docs/EXEC-PROBE.md).

The agent is wired end to end on device — Keystore, TLS, streaming, tool loop —
and verified as far as a valid key allows: with a deliberately invalid key the
API answers `401 authentication_error`, which exercises everything except the
credential itself. **A real turn has not been run; that needs your key.**

What is not done: the UI is a shell (panes are placeholders, and the agent is
driven by intent extras rather than a chat view), and the Terminal pane is
desktop-only because `freya-terminal` does not cross-compile to Android (§2.1).

```sh
apps/android/run-app.sh --logs    # build, install, launch, tail the logs
```

## Layout

```
crates/
  mc-core      session & project model, event bus        no deps on the rest
  mc-sandbox   proot, rootfs, Android execution rules    knows nothing of Claude
  mc-pty       interactive PTY sessions
  mc-agent     Claude Messages API loop + tool dispatch
  mc-ui        Freya components (desktop + Android)
apps/
  desktop      the fast dev loop
  android      cdylib + Gradle project
tools/
  exec-probe   answers the targetSdk >= 29 question on real hardware
```

The boundaries are drawn so that everything except `mc-ui` develops and tests on
the desktop, where iteration is seconds rather than an APK install.

## Getting started

```sh
cargo test -p mc-core -p mc-sandbox -p mc-pty -p mc-agent   # fast: no Freya
cargo run -p desktop                                        # first build compiles Skia - slow
```

`cargo test` at the workspace root pulls in Freya, and therefore Skia. The first
such build takes a long time and is cached afterwards; the four core crates above
avoid it entirely and are where nearly all the logic lives.

### Running the agent

**With a local model** — LM Studio (or any server speaking the Anthropic
Messages API) works on both paths:

```sh
# desktop
export ANTHROPIC_BASE_URL=http://localhost:1234 ANTHROPIC_MODEL=qwen/qwen3.8-27b
cargo run -p desktop -- --prompt "list the files here"

# emulator: 10.0.2.2 is the host's localhost (a phone needs your LAN address)
apps/android/agent.sh --endpoint http://10.0.2.2:1234 --model qwen/qwen3.8-27b
apps/android/agent.sh "show /etc/alpine-release"
```

No key is needed for a custom endpoint. The endpoint is stored in
SharedPreferences (it is not secret); an empty value resets to Anthropic's API.

**On the x86_64 emulator, compound shell commands fail** (`&&`, pipes):
musl calls the raw `fork` syscall, which Android's seccomp policy blocks for
64-bit apps. arm64 phones are not affected - see `docs/EXEC-PROBE.md`.

**On desktop** — the fast path, no device needed:

```sh
export ANTHROPIC_API_KEY=sk-ant-...
cargo run -p desktop -- --prompt "list the files in the current directory"
```

Runs against a host-shell sandbox instead of proot. The agent cannot tell the
difference, so streaming and tool-loop bugs surface here in seconds rather than
on a phone.

**On device** — the key is sealed by the Android Keystore, so it is stored once:

```sh
apps/android/agent.sh --key sk-ant-...            # store it (encrypted at rest)
apps/android/agent.sh "list the files in /root"   # run a turn, stream the logs
```

Use the script rather than `adb shell am start` directly: `adb shell` passes the
command line through the *device's* shell, so an unquoted multi-word `--es prompt`
arrives silently truncated at the first space.

The key is encrypted with an AES-GCM key held in the Android Keystore — only
ciphertext touches app storage, and the plaintext exists only in memory for the
life of the process. It is logged redacted (`sk-ant-a… 49 chars`), never whole.

`mc-agent` talks to the Messages API over raw HTTP — there is no official
Anthropic SDK for Rust, so the wire format is ours: see `crates/mc-agent/src/wire.rs`,
particularly `StreamAccumulator`, which rebuilds messages from SSE deltas while
keeping thinking signatures and unknown block types intact.

## Releases

Tag a commit `v0.2.0` and push it, or publish a release for that tag, and
[`.github/workflows/release-apk.yml`](.github/workflows/release-apk.yml) builds
the APK and attaches it to the release. The version comes from the tag.

The APK is arm64 only. `x86_64` builds, but the sandbox cannot `fork` there
(it is an emulator-only ABI — see `docs/EXEC-PROBE.md`), so it is not worth the
download; a manual run of the workflow can ask for both.

**To get properly signed builds**, add four repository secrets:

```sh
keytool -genkeypair -keystore release.jks -alias mobile-coder \
  -keyalg RSA -keysize 2048 -validity 10000
base64 -w0 release.jks     # → ANDROID_KEYSTORE_BASE64
```

| Secret | What it is |
|---|---|
| `ANDROID_KEYSTORE_BASE64` | the keystore above, base64 |
| `ANDROID_KEYSTORE_PASSWORD` | its store password |
| `ANDROID_KEY_ALIAS` | the key alias (`mobile-coder` above) |
| `ANDROID_KEY_PASSWORD` | the key password, if it differs from the store's |

Without them the workflow still produces an APK, debug-signed: it installs and
runs, but Android will refuse to upgrade over a copy signed with a different
key. Keep the keystore file somewhere safe — losing it means every future
release is a fresh install for anyone who already has the app.

## The constraint worth knowing before you read the code

An Android app targeting API 29+ may not `exec()` files in its own data
directory. proot does not fix this — proot translates *paths*; the kernel still
performs the `execve` and still refuses. See
[`crates/mc-sandbox/src/exec.rs`](crates/mc-sandbox/src/exec.rs), which documents
the rule and implements the documented escape, and `docs/EXEC-PROBE.md`, which
measures whether that escape actually carries a Linux userland.

## Licence

GPL-3.0 license.
