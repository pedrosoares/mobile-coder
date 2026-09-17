#!/usr/bin/env bash
# Set the API key and/or run one agent turn on the connected device.
#
#   apps/android/agent.sh --key sk-ant-...        store a key (sealed by the Keystore)
#   apps/android/agent.sh --endpoint http://10.0.2.2:1234 --model qwen/qwen3.8-27b
#                                                 use LM Studio on the host (emulator)
#   apps/android/agent.sh "list the files in /root"   run one turn
#   apps/android/agent.sh --key sk-... "prompt"       both
#
# Exists mainly to get the quoting right. `adb shell` passes the command line
# through the *device's* shell, so an unquoted `--es prompt "two words"` arrives
# truncated at the first space - silently, and the app just sees "two".
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"
pkg="net.pedrosoares.mobilecoder"
# shellcheck source=/dev/null
[ -f "$root/env.sh" ] && . "$root/env.sh"

key=""; endpoint=""; model=""
while [ $# -gt 0 ]; do
  case "$1" in
    --key)      key="${2:?--key needs a value}"; shift 2 ;;
    --endpoint) endpoint="${2?--endpoint needs a value}"; shift 2 ;;
    --model)    model="${2?--model needs a value}"; shift 2 ;;
    *) break ;;
  esac
done
prompt="${1:-}"

[ -n "$key$endpoint$model$prompt" ] || { sed -n '2,11p' "$0" | sed 's/^# \?//' >&2; exit 1; }

# Escape single quotes for the device shell: ' -> '\''
q() { printf "'%s'" "$(printf '%s' "$1" | sed "s/'/'\\\\''/g")"; }

args="am start -n $pkg/.MainActivity"
[ -n "$key" ]      && args="$args --es api_key $(q "$key")"
[ -n "$endpoint" ] && args="$args --es base_url $(q "$endpoint")"
[ -n "$model" ]    && args="$args --es model $(q "$model")"
[ -n "$prompt" ] && args="$args --es prompt $(q "$prompt")"

adb logcat -c
adb shell "$args" >/dev/null
[ -n "$key" ] && echo "==> key stored (sealed by the Android Keystore)"
[ -n "$endpoint$model" ] && echo "==> endpoint set: ${endpoint:-<unchanged>} model=${model:-<unchanged>}"

if [ -n "$prompt" ]; then
  echo "==> running: $prompt"
  echo "==> streaming (ctrl-c to stop)"
  adb logcat -s mobile-coder:V
fi
