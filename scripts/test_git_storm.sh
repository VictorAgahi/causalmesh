#!/usr/bin/env bash
# scripts/test_git_storm.sh — Plan 4 step 4.2 exit check: a Git operation never
# gets a half-checked-out index installed.
#
# Builds a synthetic Git repository, creates a branch that rewrites every
# generated file, then runs `git checkout` of that branch while a live
# `mesh-mcp run --standalone` answers `smart_search` in a loop. Passes when:
#   1. the watcher opened a Git hold during the checkout, and tool answers
#      given during it carried the "Git operation in progress" note;
#   2. no generation was installed between the hold opening and its release;
#   3. exactly one generation was installed after the checkout started;
#   4. that generation's fingerprint equals a cold
#      `mesh-mcp graph --format fingerprint` of the same directory (the
#      fingerprint embeds absolute paths, so both runs index the same path).
#
# usage: scripts/test_git_storm.sh [work_dir]
#   work_dir  scratch directory, created, must not exist yet or be empty
#             (default: a fresh mktemp -d under /tmp). It must not sit under a
#             git-ignored path of another repository (e.g. target/).
# env:
#   MESH_MCP_BIN  binary to test (default: target/release/mesh-mcp, built if missing)
#   FILES         files rewritten by the checkout (default: 3000)
#   KEEP_WORK=1   keep work_dir afterwards (default: removed on success)
#
# Isolation: HOME points inside work_dir, so the persistent index cache and the
# audit DB never touch the user's; --standalone never talks to a meshd.
set -Eeuo pipefail

on_err() {
  local rc=$? line=$1 cmd=$2
  echo "✖ test_git_storm.sh: command failed (exit $rc) at line $line: $cmd" >&2
  exit "$rc"
}
trap 'on_err "$LINENO" "$BASH_COMMAND"' ERR

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BIN="${MESH_MCP_BIN:-$REPO_ROOT/target/release/mesh-mcp}"
FILES="${FILES:-3000}"

case "$FILES" in
  '' | *[!0-9]*) echo "✖ FILES must be a positive integer, got '$FILES'" >&2; exit 1 ;;
esac
if [[ ! -x "$BIN" ]]; then
  echo "👉 Building release binary..."
  cargo build --release -p mesh-server --manifest-path "$REPO_ROOT/Cargo.toml"
fi
case "$BIN" in
  /*) ;;
  *) BIN="$(cd "$(dirname "$BIN")" && pwd)/$(basename "$BIN")" ;;
esac

WORK="${1:-$(mktemp -d /tmp/mesh-git-storm.XXXXXX)}"
mkdir -p "$WORK"
if [[ -n "$(ls -A "$WORK")" ]]; then
  echo "✖ work_dir $WORK is not empty" >&2
  exit 1
fi
WORK="$(cd "$WORK" && pwd -P)"
REPO="$WORK/repo"
export HOME="$WORK/home"
mkdir -p "$HOME" "$REPO"

git_() { git -C "$REPO" -c user.email=storm@test -c user.name=storm -c init.defaultBranch=main "$@"; }

echo "👉 Generating $FILES files in $REPO"
python3 "$SCRIPT_DIR/bench/gen_synthetic.py" "$REPO" "$FILES" --services 30 --contracts >/dev/null
printf '[workspace]\nname = "git-storm"\nversion = "1"\nroots = ["."]\n' > "$REPO/mesh-mcp.toml"
git_ init -q
git_ add -A
git_ commit -qm "main"

# The arrival branch rewrites every generated source file (renamed symbols, so
# the two branches' graphs really differ).
git_ checkout -qb feature
find "$REPO/services" -type f -name 'gen_*' -exec perl -pi -e 's/(Service|Handler|Worker|Component)(\d+)/${1}Feat$2/g' {} +
git_ add -A
git_ commit -qm "feature"
CHANGED="$(git_ diff --name-only main feature | wc -l | tr -d ' ')"
git_ checkout -q main
echo "👉 Checkout main → feature rewrites $CHANGED files"
if [[ "$CHANGED" -lt "$FILES" ]]; then
  echo "✖ expected at least $FILES changed files, got $CHANGED" >&2
  exit 1
fi

LOG="$WORK/server.log"
SUMMARY="$WORK/summary.json"
DRIVER="$WORK/driver.py"
cat > "$DRIVER" <<'PY'
import json, os, re, subprocess, sys, threading, time

bin_, repo, log_path, summary_path = sys.argv[1:5]
ansi = re.compile(r"\x1b\[[0-9;]*m")

def log_text():
    with open(log_path, errors="replace") as f:
        return ansi.sub("", f.read())

env = dict(os.environ, RUST_LOG="info,mesh::watcher=debug")
log = open(log_path, "w")
proc = subprocess.Popen([bin_, "--config", os.path.join(repo, "mesh-mcp.toml"), "run", "--standalone"],
                        cwd=repo, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log,
                        text=True, bufsize=1, env=env)
lock = threading.Lock()
next_id = [0]

def call(method, params):
    with lock:
        next_id[0] += 1
        proc.stdin.write(json.dumps({"jsonrpc": "2.0", "id": next_id[0], "method": method, "params": params}) + "\n")
        proc.stdin.flush()
        line = proc.stdout.readline()
    if not line:
        raise SystemExit("server closed stdout; see " + log_path)
    return json.loads(line)

call("initialize", {"protocolVersion": "2024-11-05", "capabilities": {}, "clientInfo": {"name": "storm", "version": "0"}})

# Ready = watcher registered + the post-spawn catch-up reload done, then quiet.
deadline = time.time() + 300
while "watching root" not in log_text():
    if time.time() > deadline:
        raise SystemExit("watcher never started; see " + log_path)
    time.sleep(0.2)
time.sleep(3)

marker = len(log_text())
answers = []
stop = threading.Event()

def search_loop():
    while not stop.is_set():
        t = time.time()
        resp = call("tools/call", {"name": "smart_search", "arguments": {"query": "Service1", "scope": repo}})
        text = resp.get("result", {}).get("content", [{}])[0].get("text", "")
        m = re.search(r"Git operation in progress.*?generation (\d+)", text, re.S)
        answers.append({"t": t, "note_generation": int(m.group(1)) if m else None})

th = threading.Thread(target=search_loop)
th.start()
time.sleep(1)
t0 = time.time()
subprocess.run(["git", "-C", repo, "checkout", "-q", "feature"], check=True)
t1 = time.time()
# Settle + one full reload of the arrival branch, generously.
deadline = time.time() + 120
while "Git operation finished" not in log_text()[marker:] and time.time() < deadline:
    time.sleep(0.2)
while time.time() < deadline:
    tail = log_text()[marker:]
    after = tail.split("Git operation finished", 1)[-1]
    if "Installed generation" in after:
        break
    time.sleep(0.2)
time.sleep(5)
stop.set()
th.join()
proc.stdin.close()
try:
    proc.wait(timeout=30)
except subprocess.TimeoutExpired:
    proc.kill()
log.close()

tail = log_text()[marker:].splitlines()
events = []
for line in tail:
    if "Git operation in progress: holding reloads" in line:
        events.append(("hold", None))
    elif "Git operation finished" in line or "Git operation still in progress after" in line:
        events.append(("release", None))
    m = re.search(r"reload \(gen (\d+)\)", line)
    if m:
        events.append(("gen", int(m.group(1))))
    m = re.search(r"Installed generation (\d+) fingerprint ([0-9a-f]+)", line)
    if m:
        events.append(("fp", (int(m.group(1)), m.group(2))))

json.dump({"checkout_seconds": round(t1 - t0, 3), "events": events,
           "answers": len(answers),
           "noted_answers": sum(1 for a in answers if a["note_generation"] is not None),
           "note_generations": sorted({a["note_generation"] for a in answers if a["note_generation"] is not None})},
          open(summary_path, "w"))
PY

echo "👉 Checking out under load (smart_search loop against a live server)"
python3 "$DRIVER" "$BIN" "$REPO" "$LOG" "$SUMMARY"

echo "👉 Cold index of the arrival branch, same directory"
COLD="$WORK/cold.txt"
if ! (cd "$REPO" && "$BIN" --config "$REPO/mesh-mcp.toml" graph --format fingerprint -o "$COLD" 2>"$WORK/cold.log"); then
  echo "✖ cold fingerprint failed:" >&2
  cat "$WORK/cold.log" >&2
  exit 1
fi

python3 - "$SUMMARY" "$COLD" <<'PY'
import json, sys
s = json.load(open(sys.argv[1]))
cold = open(sys.argv[2]).readline().split()[-1]
ev = s["events"]
print(f"   checkout took {s['checkout_seconds']}s; {s['answers']} smart_search answers, "
      f"{s['noted_answers']} carried the Git note (generations {s['note_generations']})")
print("   log events:", " ".join(k if v is None else f"{k}={v}" if k != 'fp' else f"fp=gen{v[0]}" for k, v in ev))
fail = []
kinds = [k for k, _ in ev]
if "hold" not in kinds:
    fail.append("no Git hold was opened during the checkout")
if "release" not in kinds:
    fail.append("the Git hold was never released")
if "hold" in kinds and "release" in kinds:
    during = kinds[kinds.index("hold"):kinds.index("release")]
    if "gen" in during:
        fail.append("a generation was installed while the Git operation was in progress")
gens = [v for k, v in ev if k == "gen"]
if len(gens) != 1:
    fail.append(f"expected exactly one generation after the checkout, got {gens}")
fps = [v for k, v in ev if k == "fp"]
live = fps[-1][1] if fps else None
print(f"   live fingerprint (gen {fps[-1][0] if fps else '?'}): {live}")
print(f"   cold fingerprint:          {cold}")
if live != cold:
    fail.append("live index fingerprint differs from a cold index of the arrival branch")
if s["noted_answers"] == 0:
    fail.append("no tool answer carried the 'Git operation in progress' note")
if fail:
    for f in fail:
        print("✖", f)
    sys.exit(1)
print("✔ git storm: one generation after the checkout, none during it, fingerprint matches a cold index")
PY

if [[ "${KEEP_WORK:-0}" != "1" && -z "${1:-}" ]]; then
  rm -rf "$WORK"
else
  echo "   work dir kept: $WORK"
fi
