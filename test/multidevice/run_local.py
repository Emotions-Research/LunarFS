#!/usr/bin/env python3
"""No-Docker multi-device test for LunarFS: 1 local server + 3 device home dirs,
against the current-source `lunar` binary. Proves cross-device sync, concurrent
edit behavior, auto-sync convergence, latency measurement, CAS conflict refs,
and benchmarks vs git/rsync."""
import os, sys, time, json, shutil, sqlite3, hashlib, secrets, subprocess, threading, tempfile
import urllib.request, urllib.error

import pathlib, socket
REPO = str(pathlib.Path(__file__).resolve().parents[2])
L = f"{REPO}/target/debug/lunar"
ROOT = "/tmp/mdtest"

def _free_port():
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        s.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        return s.getsockname()[1]

ADDR = f"127.0.0.1:{_free_port()}"
BASE = f"http://{ADDR}"
DB = f"{ROOT}/lunar.db"

def now_ms(): return time.time() * 1000.0
def sh(args, env=None, cwd=None):
    e = dict(os.environ); e.update(env or {})
    return subprocess.run(args, env=e, cwd=cwd, capture_output=True, text=True)
def dev_env(d): return {"HOME": f"{ROOT}/{d}", "LUNAR_BASE_URL": BASE, "LUNAR_TOKEN": TOKEN, "LUNAR_ORG": "team"}
def lunar(d, *a): return sh([L, *a], env=dev_env(d))

def http(path, token=None):
    req = urllib.request.Request(BASE + path)
    if token: req.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=5) as r: return r.status, r.read().decode()
    except urllib.error.HTTPError as e: return e.code, e.read().decode()
    except Exception as e: return 0, str(e)

def http_put(path, token, body_dict):
    data = json.dumps(body_dict).encode()
    req = urllib.request.Request(BASE + path, data=data, method="PUT")
    req.add_header("Authorization", "Bearer " + token)
    req.add_header("Content-Type", "application/json")
    try:
        with urllib.request.urlopen(req, timeout=5) as r: return r.status, r.read().decode()
    except urllib.error.HTTPError as e: return e.code, e.read().decode()
    except Exception as e: return 0, str(e)

def http_put_blob(hash_hex, workspace, token, data):
    """PUT a raw blob to the server with workspace auth."""
    req = urllib.request.Request(f"{BASE}/v1/blob/{hash_hex}?workspace={workspace}", data=data, method="PUT")
    req.add_header("Authorization", "Bearer " + token)
    req.add_header("Content-Type", "application/octet-stream")
    try:
        with urllib.request.urlopen(req, timeout=5) as r: return r.status, r.read().decode()
    except urllib.error.HTTPError as e: return e.code, e.read().decode()
    except Exception as e: return 0, str(e)

def head_blob(hash_hex, workspace, token):
    """HEAD a blob on the server; returns HTTP status."""
    req = urllib.request.Request(f"{BASE}/v1/blob/{hash_hex}?workspace={workspace}", method="HEAD")
    req.add_header("Authorization", "Bearer " + token)
    try:
        with urllib.request.urlopen(req, timeout=5) as r: return r.status
    except urllib.error.HTTPError as e: return e.code
    except Exception: return 0

# ---- reset + start server ----
if os.path.isdir(ROOT): shutil.rmtree(ROOT)
os.makedirs(f"{ROOT}/store")
for d in ("dev1", "dev2", "dev3"): os.makedirs(f"{ROOT}/{d}")
srv = subprocess.Popen([L, "serve", "--store", f"local:{ROOT}/store", "--addr", ADDR, "--db", DB],
                       stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
for _ in range(60):
    code, _ = http("/v1/workspaces")
    if code: break
    time.sleep(0.5)
print(f"server up (HTTP {code})")

# ---- seed identity DB ----
TOKEN = "ddb_" + secrets.token_hex(32)
HASH = hashlib.sha256(TOKEN.encode()).digest()
ts = 1782700000
c = sqlite3.connect(DB)
c.execute("INSERT INTO users(external_clerk_id, created_at) VALUES(NULL, ?)", (ts,))
c.execute("INSERT INTO organizations(slug, created_at) VALUES('team', ?)", (ts,))
c.execute("INSERT INTO memberships(user_id, org_id, role) VALUES(1,1,'owner')")
c.execute("INSERT INTO workspaces(name, owner_kind, owner_id, created_at) VALUES('demo','org',1,?)", (ts,))
c.execute("INSERT INTO workspaces(name, owner_kind, owner_id, created_at) VALUES('autosync','org',1,?)", (ts,))
c.execute("INSERT INTO workspaces(name, owner_kind, owner_id, created_at) VALUES('conflict','org',1,?)", (ts,))
c.execute("INSERT INTO api_tokens(principal_kind,principal_id,token_hash,scope,created_at,expires_at,revoked_at) VALUES('user','1',?,NULL,?,NULL,NULL)", (HASH, ts))
c.execute("INSERT INTO acl_grants(principal_kind,principal_id,workspace_id,path_prefix,permission,created_at) VALUES('org','1',1,'/','write',?)", (ts,))
c.execute("INSERT INTO acl_grants(principal_kind,principal_id,workspace_id,path_prefix,permission,created_at) VALUES('org','1',2,'/','write',?)", (ts,))
c.execute("INSERT INTO acl_grants(principal_kind,principal_id,workspace_id,path_prefix,permission,created_at) VALUES('org','1',3,'/','write',?)", (ts,))
c.commit(); c.close()
code, body = http("/v1/ref/demo", TOKEN)
print(f"seeded; auth check /v1/ref/demo -> HTTP {code} (200/404 = authed)")

def make_repo(path, files):
    if os.path.isdir(path): shutil.rmtree(path)
    os.makedirs(path + "/src")
    for name, content in files.items():
        fp = os.path.join(path, name); os.makedirs(os.path.dirname(fp), exist_ok=True)
        open(fp, "w").write(content)

def ingest(d, path):
    r = lunar(d, "ingest", path); return r.stdout.strip()
def push(d, root): return lunar(d, "push", "team/demo", root)
def pull(d): return lunar(d, "pull", "team/demo")
def push_to(d, ws, root): return lunar(d, "push", ws, root)
def pull_from(d, ws): return lunar(d, "pull", ws)
def server_ref():
    code, body = http("/v1/ref/demo", TOKEN)
    try: return json.loads(body).get("root") or json.loads(body).get("ref") or body.strip()
    except Exception: return f"HTTP{code}:{body[:40]}"

R = {"scenarios": {}, "bench": {}}

# ===== Scenario 1: fan-out sync =====
base_files = {"README.md": "# Team Project\nv1\n", "src/index.js": "export const v='1.0.0';\n", "src/util.js": "export const u=1;\n"}
make_repo(f"{ROOT}/work1", base_files)
R1 = ingest("dev1", f"{ROOT}/work1")
push("dev1", R1)
p2, p3 = pull("dev2"), pull("dev3")
ref_after = server_ref()
s1_ok = (R1 and p2.returncode == 0 and p3.returncode == 0 and ref_after == R1)
R["scenarios"]["1_fanout"] = {"ok": s1_ok, "root": R1[:16], "dev2_pull_rc": p2.returncode, "dev3_pull_rc": p3.returncode, "server_ref": str(ref_after)[:16]}
print(f"\n[S1 fan-out] dev1 push root {R1[:12]}; dev2/dev3 pull rc={p2.returncode}/{p3.returncode}; server ref={str(ref_after)[:12]}; converged={ref_after==R1}")

# ===== Scenario 2: concurrent disjoint edits =====
roots = {}
for d, fname in (("dev1", "src/a.js"), ("dev2", "src/b.js"), ("dev3", "src/c.js")):
    f = dict(base_files); f[fname] = f"// edit by {d}\n"
    make_repo(f"{ROOT}/work_{d}", f)
    roots[d] = ingest(d, f"{ROOT}/work_{d}")
import threading
threads = [threading.Thread(target=push, args=(d, roots[d])) for d in roots]
[t.start() for t in threads]; [t.join() for t in threads]
ref2 = server_ref()
preserved = {d: http(f"/v1/blob/{roots[d]}", TOKEN)[0] for d in roots}
winner = [d for d, r in roots.items() if r == ref2]
s2_ok = ref2 in roots.values() and len(set(roots.values())) == 3
R["scenarios"]["2_disjoint"] = {"ok": s2_ok, "winner": winner, "server_ref": str(ref2)[:16], "roots": {d: r[:12] for d, r in roots.items()}, "blob_status": preserved}
print(f"[S2 disjoint] 3 concurrent pushes; winner ref={str(ref2)[:12]} ({winner}); all roots distinct={len(set(roots.values()))==3}; blob-fetch={preserved}")

# ===== Scenario 3: same-file conflict =====
for d, val in (("dev1", "ONE"), ("dev2", "TWO")):
    f = dict(base_files); f["README.md"] = f"# Team Project\nedited-{val}\n"
    make_repo(f"{ROOT}/conf_{d}", f)
    roots[d] = ingest(d, f"{ROOT}/conf_{d}")
t1 = threading.Thread(target=push, args=("dev1", roots["dev1"]))
t2 = threading.Thread(target=push, args=("dev2", roots["dev2"]))
t1.start(); t2.start(); t1.join(); t2.join()
ref3 = server_ref()
both_present = {d: http(f"/v1/blob/{roots[d]}", TOKEN)[0] for d in ("dev1", "dev2")}
s3_ok = ref3 in (roots["dev1"], roots["dev2"])
R["scenarios"]["3_samefile"] = {"ok": s3_ok, "server_ref": str(ref3)[:16], "dev1_root": roots["dev1"][:12], "dev2_root": roots["dev2"][:12], "both_blobs": both_present}
print(f"[S3 same-file] dev1 vs dev2 edit README; winner ref={str(ref3)[:12]}; both versions in CAS={both_present}")

# ===== Scenario 4: convergence =====
pf = [pull(d) for d in ("dev1", "dev2", "dev3")]
s4_ok = all(p.returncode == 0 for p in pf)
R["scenarios"]["4_convergence"] = {"ok": s4_ok, "pull_rcs": [p.returncode for p in pf], "final_ref": str(server_ref())[:16]}
print(f"[S4 convergence] all devices pull rc={[p.returncode for p in pf]}; final ref={str(server_ref())[:12]}")

# ===== Helpers for CAS tree walking (used by S5 and S6) =====

MODE_DIR = 0o040000

def read_cas_blob(home, hash_hex):
    """Read a raw blob from the device-local CAS at home/.lunar/cas/<prefix>/<rest>."""
    prefix, rest = hash_hex[:2], hash_hex[2:]
    path = os.path.join(home, ".lunar", "cas", prefix, rest)
    with open(path, "rb") as fh:
        return fh.read()

def walk_cas_tree(home, root_hex, prefix=""):
    """Yield (rel_path, bytes) for every file reachable from root_hex in local CAS.
    Bounded to MAX_ENTRIES total across the entire tree to prevent runaway.
    """
    MAX_ENTRIES = 65536
    blob = read_cas_blob(home, root_hex)
    pos = 0
    entries = []
    while pos < len(blob):
        if len(entries) >= MAX_ENTRIES:
            break
        mode = int.from_bytes(blob[pos:pos+4], "little")
        name_len = int.from_bytes(blob[pos+4:pos+8], "little")
        pos += 8
        name = blob[pos:pos+name_len].decode("utf-8")
        pos += name_len
        entry_hash = blob[pos:pos+32].hex()
        pos += 32
        entries.append((mode, name, entry_hash))
    for (mode, name, entry_hash) in sorted(entries, key=lambda x: x[1]):
        rel = f"{prefix}/{name}" if prefix else name
        if mode == MODE_DIR:
            yield from walk_cas_tree(home, entry_hash, rel)
        else:
            yield rel, read_cas_blob(home, entry_hash)

def collect_server_store_blobs(home, root_hex, is_tree=True):
    """Return dict {hash_hex: bytes} for all blobs reachable from root_hex in local CAS.
    Uses a bounded stack (max 65536 nodes) to prevent infinite loops.
    """
    MAX_NODES = 65536
    blobs = {}
    stack = [(root_hex, is_tree)]
    visited = set()
    while stack:
        if len(visited) >= MAX_NODES:
            break
        h, tree = stack.pop()
        if h in visited:
            continue
        visited.add(h)
        try:
            data = read_cas_blob(home, h)
        except FileNotFoundError:
            continue
        blobs[h] = data
        if not tree:
            continue
        pos = 0
        while pos < len(data):
            mode = int.from_bytes(data[pos:pos+4], "little")
            name_len = int.from_bytes(data[pos+4:pos+8], "little")
            pos += 8
            pos += name_len
            child_hex = data[pos:pos+32].hex()
            pos += 32
            stack.append((child_hex, mode == MODE_DIR))
    return blobs

def fs_sha256_map(dirpath, skip=(".lunar", ".git")):
    """Walk filesystem, return {rel_path: sha256_hex} excluding skip dirs."""
    result = {}
    for dp, dirs, files in os.walk(dirpath):
        dirs[:] = sorted(d for d in dirs if d not in skip)
        for fname in sorted(files):
            fp = os.path.join(dp, fname)
            rel = os.path.relpath(fp, dirpath)
            with open(fp, "rb") as fh:
                result[rel] = hashlib.sha256(fh.read()).hexdigest()
    return result

def cas_sha256_map(home, root_hex):
    """Walk CAS tree from root_hex, return {rel_path: sha256_hex}."""
    return {rel: hashlib.sha256(content).hexdigest()
            for rel, content in walk_cas_tree(home, root_hex)}

# ===== Scenario 5: Auto-sync convergence + propagation latency =====
# Device A uses `lunar sync --once` to propagate file changes to the server
# without ever calling `lunar push`. B converges via `lunar pull`.
# Convergence verified by comparing path-set + per-file sha256 between A's
# filesystem and B's materialized CAS tree. Latency = wall-clock from last
# file write to sync completion.

AUTOSYNC_WS = "team/autosync"
AUTOSYNC_WS_BARE = "autosync"

print(f"\n[S5 auto-sync] setting up initial workspace state...")

autosync_base_files = {
    "README.md": "# Auto-Sync Project\nv1\n",
    "src/main.py": "print('initial')\n",
    "delete_me.txt": "this file will be deleted by device A\n",
}
make_repo(f"{ROOT}/autosync_base", autosync_base_files)
R_sync_base = ingest("dev1", f"{ROOT}/autosync_base")
push_to("dev1", AUTOSYNC_WS, R_sync_base)

autosync_work = f"{ROOT}/autosync_work"
shutil.copytree(f"{ROOT}/autosync_base", autosync_work)

# Apply file changes on A's workspace - NO lunar push anywhere
with open(os.path.join(autosync_work, "new_file.txt"), "w") as fh:
    fh.write("brand new file created by device A\n")
with open(os.path.join(autosync_work, "README.md"), "w") as fh:
    fh.write("# Auto-Sync Project\nv2 (modified by device A)\n")
os.remove(os.path.join(autosync_work, "delete_me.txt"))
t_write_done = time.monotonic()

# `lunar sync --once` snapshot + CAS push (not lunar push). Times the full pipeline.
t_sync_start = time.monotonic()
sync_r = sh([L, "sync", AUTOSYNC_WS, autosync_work, "--once"], env=dev_env("dev1"))
t_sync_done = time.monotonic()
latency_s = round(t_sync_done - t_write_done, 3)

# Check server ref advanced
sc_ref, sb_ref = http(f"/v1/ref/{AUTOSYNC_WS_BARE}", TOKEN)
try:
    new_server_ref = json.loads(sb_ref).get("root", "")
except Exception:
    new_server_ref = ""

# Drive B to pull and converge
b_pull_r = pull_from("dev2", AUTOSYNC_WS)
b_root_hex = None
for line in b_pull_r.stdout.splitlines():
    if "root=" in line:
        b_root_hex = line.split("root=")[1].strip()

# Convergence: A's filesystem vs B's CAS tree, both as sha256 maps
a_map = fs_sha256_map(autosync_work)
b_map = cas_sha256_map(f"{ROOT}/dev2", b_root_hex) if b_root_hex else {}

paths_match = set(a_map.keys()) == set(b_map.keys())
hashes_match = a_map == b_map
ref_advanced = bool(new_server_ref) and new_server_ref != R_sync_base
converged = (ref_advanced and b_root_hex == new_server_ref and paths_match and hashes_match)

s5_ok = converged
R["scenarios"]["5_autosync"] = {
    "ok": s5_ok,
    "sync_rc": sync_r.returncode,
    "ref_advanced": ref_advanced,
    "b_converged_to_new_ref": b_root_hex == new_server_ref if new_server_ref else False,
    "paths_match": paths_match,
    "hashes_match": hashes_match,
    "latency_s": latency_s,
    "a_paths": sorted(a_map.keys()),
    "b_paths": sorted(b_map.keys()),
}
print(f"[S5 auto-sync] sync_rc={sync_r.returncode}; ref_advanced={ref_advanced}; "
      f"B_converged={b_root_hex==new_server_ref if new_server_ref else False}; "
      f"paths_match={paths_match}; hashes_match={hashes_match}; latency={latency_s}s")

# ===== Scenario 6: Concurrent-edit CAS conflict =====
# Prove that when B pushes a root whose expected_root is stale (server has advanced
# past it), the server (a) rejects with 409, (b) saves B's root as a conflict ref
# in the object store, and (c) leaves A's root as the workspace ref (no clobber).
# Both A's and B's content survive and B's blobs are recoverable.

CONFLICT_WS_BARE = "conflict"
CONFLICT_WS = "team/conflict"

print(f"\n[S6 CAS conflict] setting up initial state...")

# Initial base content (shared path modified independently by A and B)
conflict_base = {"shared.txt": "initial shared content\n", "other.txt": "unmodified\n"}
make_repo(f"{ROOT}/conflict_base", conflict_base)
R_conflict_base = ingest("dev1", f"{ROOT}/conflict_base")
push_to("dev1", CONFLICT_WS, R_conflict_base)
R0_hex = R_conflict_base

# Device A edits shared.txt, pushes R_A via `lunar push` (CAS-aware, seeds from server)
conflict_a = dict(conflict_base)
conflict_a["shared.txt"] = "device A version of shared.txt\n"
make_repo(f"{ROOT}/conflict_a", conflict_a)
R_A_hex = ingest("dev1", f"{ROOT}/conflict_a")
push_to("dev1", CONFLICT_WS, R_A_hex)  # CAS push: expected=R0 -> commits
sc_ref_a, sb_ref_a = http(f"/v1/ref/{CONFLICT_WS_BARE}", TOKEN)
try:
    server_after_a = json.loads(sb_ref_a).get("root", "")
except Exception:
    server_after_a = ""
a_committed = (server_after_a == R_A_hex)

# Device B independently edits shared.txt (diverged from R0, never saw A's push)
conflict_b = dict(conflict_base)
conflict_b["shared.txt"] = "device B version of shared.txt\n"
make_repo(f"{ROOT}/conflict_b", conflict_b)
R_B_hex = ingest("dev2", f"{ROOT}/conflict_b")

# Upload B's blobs to the server before the CAS attempt (so they survive the 409)
b_blobs = collect_server_store_blobs(f"{ROOT}/dev2", R_B_hex, is_tree=True)
blob_upload_ok = True
for blob_hex, blob_data in b_blobs.items():
    sc, _ = http_put_blob(blob_hex, CONFLICT_WS_BARE, TOKEN, blob_data)
    if sc not in (200, 201, 409):  # 409 = already exists (idempotent)
        blob_upload_ok = False

# Simulate B's stale CAS push: expected_root=R0 (server is actually at R_A -> 409)
conflict_sc, conflict_sb = http_put(
    f"/v1/ref/{CONFLICT_WS_BARE}", TOKEN,
    {"root": R_B_hex, "expected_root": R0_hex}
)
conflict_data = {}
try:
    conflict_data = json.loads(conflict_sb)
except Exception:
    pass
conflict_ref_name = conflict_data.get("conflict_ref", "")
conflict_current_root = conflict_data.get("current_root", "")

# Verify the conflict ref naming convention: <ws>@conflict-<first 8 hex chars of R_B>
expected_conflict_ref = f"{CONFLICT_WS_BARE}@conflict-{R_B_hex[:8]}"

# Verify B's root is saved in the conflict ref by reading the object store file directly.
# The server stores it at {ROOT}/store/ref/<conflict_ref_name>.
store_conflict_ref_path = os.path.join(ROOT, "store", "ref", conflict_ref_name)
cref_root = ""
try:
    with open(store_conflict_ref_path, "rb") as fh:
        cref_root = json.loads(fh.read()).get("root", "")
except (FileNotFoundError, json.JSONDecodeError):
    cref_root = ""

# Verify main ref is still A's (winner not clobbered)
main_sc, main_sb = http(f"/v1/ref/{CONFLICT_WS_BARE}", TOKEN)
try:
    main_root_after = json.loads(main_sb).get("root", "")
except Exception:
    main_root_after = ""

# Spot-check that B's blobs exist on the server via HEAD
b_blob_hexes = list(b_blobs.keys())[:5]
b_blobs_on_server = all(head_blob(h, CONFLICT_WS_BARE, TOKEN) in (200, 204) for h in b_blob_hexes)

s6_ok = (
    conflict_sc == 409
    and conflict_ref_name == expected_conflict_ref
    and cref_root == R_B_hex
    and main_root_after == R_A_hex
    and a_committed
    and b_blobs_on_server
)
R["scenarios"]["6_cas_conflict"] = {
    "ok": s6_ok,
    "conflict_http_status": conflict_sc,
    "conflict_ref_matches_convention": conflict_ref_name == expected_conflict_ref,
    "conflict_ref_name": conflict_ref_name,
    "cref_root_is_R_B": cref_root == R_B_hex,
    "main_ref_is_R_A": main_root_after == R_A_hex,
    "a_committed": a_committed,
    "b_blobs_on_server": b_blobs_on_server,
    "blob_upload_ok": blob_upload_ok,
    "R_A_prefix": R_A_hex[:16],
    "R_B_prefix": R_B_hex[:16],
}
print(f"[S6 CAS conflict] 409_received={conflict_sc==409}; "
      f"cref={conflict_ref_name}; cref_root_is_R_B={cref_root==R_B_hex}; "
      f"main_is_A={main_root_after==R_A_hex}; B_blobs_ok={b_blobs_on_server}")

# ===== Speed benchmark: LunarFS vs git/rsync =====
bench_src = f"{ROOT}/benchrepo"
if os.path.isdir(bench_src): shutil.rmtree(bench_src)
os.makedirs(bench_src + "/node_modules")
for i in range(400):
    d = f"{bench_src}/node_modules/pkg{i}"; os.makedirs(d)
    open(f"{d}/index.js", "w").write(f"module.exports={{id:{i}}};\n" * 8)
    open(f"{d}/package.json", "w").write(json.dumps({"name": f"pkg{i}", "version": "1.0.0"}))
for i in range(5):
    open(f"{bench_src}/big{i}.bin", "w").write("x" * (200 * 1024))
nfiles = sum(len(fs) for _, _, fs in os.walk(bench_src))
size_mb = sum(os.path.getsize(os.path.join(dp, f)) for dp, _, fs in os.walk(bench_src) for f in fs) / 1e6

def timed(fn, reps=3):
    ts = []
    for _ in range(reps):
        a = now_ms(); fn(); ts.append(now_ms() - a)
    ts.sort(); return ts[len(ts)//2]

BR = ingest("dev1", bench_src)
git_repo = f"{ROOT}/gitrepo"; shutil.copytree(bench_src, git_repo)
sh(["git", "init", "-q"], cwd=git_repo); sh(["git", "add", "-A"], cwd=git_repo)
sh(["git", "-c", "user.email=t@t.t", "-c", "user.name=t", "commit", "-qm", "init"], cwd=git_repo)
fork_n = [0]
def lunar_fork():
    fork_n[0] += 1; lunar("dev1", "ws", "fork", "--from", BR, "--label", f"b{fork_n[0]}")
wt_n = [0]
def git_worktree():
    wt_n[0] += 1; sh(["git", "worktree", "add", "-q", f"{ROOT}/wt{wt_n[0]}", "HEAD"], cwd=git_repo)
lunar_fork_ms = timed(lunar_fork)
git_wt_ms = timed(git_worktree)
push("dev1", BR)
def lunar_pull2(): shutil.rmtree(f"{ROOT}/dev2/.lunar", ignore_errors=True); pull("dev2")
def git_clone():
    t = f"{ROOT}/clone{now_ms():.0f}"; sh(["git", "clone", "-q", git_repo, t])
def rsync_copy(): sh(["rsync", "-a", bench_src + "/", f"{ROOT}/rs{now_ms():.0f}/"])
lunar_pull_ms = timed(lunar_pull2, reps=2)
git_clone_ms = timed(git_clone, reps=2)
rsync_ms = timed(rsync_copy, reps=2)

R["bench"] = {
    "repo": {"files": nfiles, "size_mb": round(size_mb, 1)},
    "fork_workspace_ms": {"lunar_ws_fork": round(lunar_fork_ms, 1), "git_worktree_add": round(git_wt_ms, 1), "speedup_x": round(git_wt_ms / max(lunar_fork_ms, 0.1), 1)},
    "distribute_2nd_machine_ms": {"lunar_push_done_pull": round(lunar_pull_ms, 1), "git_clone": round(git_clone_ms, 1), "rsync": round(rsync_ms, 1)},
}
print(f"\n[BENCH] repo: {nfiles} files, {size_mb:.1f} MB")
print(f"  fork workspace:  lunar ws fork = {lunar_fork_ms:.1f}ms  vs  git worktree = {git_wt_ms:.1f}ms  ({git_wt_ms/max(lunar_fork_ms,0.1):.0f}x faster)")
print(f"  2nd-machine get: lunar pull = {lunar_pull_ms:.1f}ms  vs  git clone = {git_clone_ms:.1f}ms  vs  rsync = {rsync_ms:.1f}ms")

srv.terminate()
print("\n" + "=" * 60)
all_ok = all(s["ok"] for s in R["scenarios"].values())
print("RESULT:", "PASS" if all_ok else "FAIL")
print(json.dumps(R, indent=2))
sys.exit(0 if all_ok else 1)
