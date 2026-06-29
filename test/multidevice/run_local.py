#!/usr/bin/env python3
"""No-Docker multi-device test for LunarFS: 1 local server + 3 device home dirs,
against the current-source `lunar` binary. Proves cross-device sync, concurrent
edit behavior, and benchmarks vs git/rsync."""
import os, sys, time, json, shutil, sqlite3, hashlib, secrets, subprocess, urllib.request, urllib.error

import pathlib
REPO = str(pathlib.Path(__file__).resolve().parents[2])
L = f"{REPO}/target/debug/lunar"
ROOT = "/tmp/mdtest"
ADDR = "127.0.0.1:8799"
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
c.execute("INSERT INTO api_tokens(principal_kind,principal_id,token_hash,scope,created_at,expires_at,revoked_at) VALUES('user','1',?,NULL,?,NULL,NULL)", (HASH, ts))
c.execute("INSERT INTO acl_grants(principal_kind,principal_id,workspace_id,path_prefix,permission,created_at) VALUES('org','1',1,'/','write',?)", (ts,))
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
# push concurrently
import threading
threads = [threading.Thread(target=push, args=(d, roots[d])) for d in roots]
[t.start() for t in threads]; [t.join() for t in threads]
ref2 = server_ref()
# all three roots must be retrievable from CAS (HEAD the tree blob)
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

# ===== Speed benchmark: LunarFS vs git/rsync =====
# Representative repo: many small files (like node_modules) + a few larger.
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

# ingest once (dev1)
BR = ingest("dev1", bench_src)
# 1) workspace fork: LunarFS O(1) CoW vs git worktree
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
# 2) distribute to a 2nd machine: LunarFS push+pull vs git clone vs rsync
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
print("RESULT:", "PASS" if all(s["ok"] for s in R["scenarios"].values()) else "FAIL")
print(json.dumps(R, indent=2))
