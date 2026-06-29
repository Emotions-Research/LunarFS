#!/bin/sh
# Speed benchmark: LunarFS operations vs git/rsync equivalents.
# All wall-clock timing runs INSIDE containers using GNU date +%s%N.
# Outputs the formatted table to stdout for inclusion in RESULTS.md.
set -eu

LUNAR_TOKEN="${LUNAR_TOKEN:?LUNAR_TOKEN not set}"
HOST_API="${HOST_API:-http://localhost:8787}"
COMPOSE_DIR="$(cd "$(dirname "$0")" && pwd)"
cd "$COMPOSE_DIR"

echo "[benchmark] starting benchmark on device-a..."

# All timing runs inside the container; key=value lines are prefixed "BENCH:"
# so they can be parsed out from any progress output.
BENCH_A=$(docker compose exec -T device-a sh << 'INNER'
set -eu

# ms <cmd_string>: run cmd, return elapsed milliseconds
ms() {
    local S E
    S=$(date +%s%N)
    eval "$1" >/dev/null 2>&1
    E=$(date +%s%N)
    echo $(( (E - S) / 1000000 ))
}

# Generate bench repo: 200 files across 20 dirs, ~10 MB total
mkdir -p /work/bench
i=0
while [ $i -lt 20 ]; do
    mkdir -p "/work/bench/dir${i}"
    j=0
    while [ $j -lt 10 ]; do
        dd if=/dev/urandom bs=5120 count=1 2>/dev/null | base64 \
            > "/work/bench/dir${i}/file${j}.txt"
        j=$((j+1))
    done
    i=$((i+1))
done
FILE_COUNT=$(find /work/bench -type f | wc -l | tr -d ' ')
BYTE_COUNT=$(du -sb /work/bench | awk '{print $1}')
MB=$(( BYTE_COUNT / 1048576 ))

# Ingest bench repo and capture root hash
ROOT_BENCH=$(lunar ingest /work/bench | tr -d '[:space:]')
MS_INGEST=$(ms "lunar ingest /work/bench")

# git repo with same content (for fork comparison)
git init /work/bench-git >/dev/null 2>&1
cp -r /work/bench/. /work/bench-git/
git -C /work/bench-git config user.email b@t && git -C /work/bench-git config user.name b
git -C /work/bench-git add . >/dev/null 2>&1
git -C /work/bench-git commit -m bench >/dev/null 2>&1

MS_FORK_LUNAR=$(ms "lunar ws fork --from $ROOT_BENCH --label bfork")
MS_GWT=$(ms "git -C /work/bench-git worktree add /tmp/git-fork HEAD")
MS_GCLONE=$(ms "git clone /work/bench-git /tmp/git-clone")

rm -rf /tmp/rsync-dest
MS_RSYNC_COLD=$(ms "rsync -a /work/bench/ /tmp/rsync-dest/")
MS_RSYNC_WARM=$(ms "rsync -a /work/bench/ /tmp/rsync-dest/")

MS_PUSH=$(ms "lunar push team/demo $ROOT_BENCH")

echo "BENCH:FILE_COUNT=$FILE_COUNT"
echo "BENCH:MB=$MB"
echo "BENCH:ROOT_BENCH=$ROOT_BENCH"
echo "BENCH:MS_INGEST=$MS_INGEST"
echo "BENCH:MS_FORK_LUNAR=$MS_FORK_LUNAR"
echo "BENCH:MS_GWT=$MS_GWT"
echo "BENCH:MS_GCLONE=$MS_GCLONE"
echo "BENCH:MS_RSYNC_COLD=$MS_RSYNC_COLD"
echo "BENCH:MS_RSYNC_WARM=$MS_RSYNC_WARM"
echo "BENCH:MS_PUSH=$MS_PUSH"
INNER
)

echo "[benchmark] device-a done; timing device-b pull..."

BENCH_B=$(docker compose exec -T device-b sh << 'INNER'
set -eu
ms() { local S E; S=$(date +%s%N); eval "$1" >/dev/null 2>&1; E=$(date +%s%N); echo $(( (E-S)/1000000 )); }
MS_PULL_COLD=$(ms "lunar pull team/demo")
MS_PULL_WARM=$(ms "lunar pull team/demo")
echo "BENCH:MS_PULL_COLD=$MS_PULL_COLD"
echo "BENCH:MS_PULL_WARM=$MS_PULL_WARM"
INNER
)

# Extract values from the BENCH: prefixed lines
field() { printf '%s\n%s' "$BENCH_A" "$BENCH_B" | grep "^BENCH:${1}=" | cut -d= -f2; }

FILE_COUNT=$(field FILE_COUNT)
MB=$(field MB)
MS_INGEST=$(field MS_INGEST)
MS_FORK_LUNAR=$(field MS_FORK_LUNAR)
MS_GWT=$(field MS_GWT)
MS_GCLONE=$(field MS_GCLONE)
MS_RSYNC_COLD=$(field MS_RSYNC_COLD)
MS_RSYNC_WARM=$(field MS_RSYNC_WARM)
MS_PUSH=$(field MS_PUSH)
MS_PULL_COLD=$(field MS_PULL_COLD)
MS_PULL_WARM=$(field MS_PULL_WARM)

# Compute speedups (X.Xf format via awk)
spd() { awk "BEGIN { s=$2; f=$1; if(f>0) printf \"%.1fx\", s/f; else print \"N/A\" }"; }
SPD_FORK_VS_GWT=$(echo | spd "$MS_FORK_LUNAR" "$MS_GWT")
SPD_FORK_VS_GCLONE=$(echo | spd "$MS_FORK_LUNAR" "$MS_GCLONE")
MS_PUSH_PULL=$((MS_PUSH + MS_PULL_COLD))
SPD_SYNC_VS_RSYNC=$(echo | spd "$MS_PUSH_PULL" "$MS_RSYNC_COLD")

cat << EOF

## Speed Benchmark

Repo: ${FILE_COUNT} files, ${MB} MB
Timing: GNU date +%s%N inside containers (no docker-exec overhead in measurements)
Network: Docker bridge (localhost, minimal latency)

### Fork / CoW comparison

| Operation                 | LunarFS            | git                         | LunarFS speedup   |
|---------------------------|--------------------|-----------------------------|-------------------|
| workspace fork (CoW)      | ${MS_FORK_LUNAR}ms | worktree add: ${MS_GWT}ms   | ${SPD_FORK_VS_GWT} vs worktree |
| workspace fork (CoW)      | ${MS_FORK_LUNAR}ms | git clone: ${MS_GCLONE}ms   | ${SPD_FORK_VS_GCLONE} vs clone |
| ingest (index tree)       | ${MS_INGEST}ms     | n/a                         | -                 |

### Sync comparison (push from device-a + pull on device-b)

| Operation                           | LunarFS (total)          | rsync local copy            | speedup |
|-------------------------------------|--------------------------|-----------------------------|---------|
| push + pull cold (0 blobs on dest)  | ${MS_PUSH}ms + ${MS_PULL_COLD}ms = ${MS_PUSH_PULL}ms | ${MS_RSYNC_COLD}ms | ${SPD_SYNC_VS_RSYNC} |
| pull warm (all blobs cached)        | ${MS_PULL_WARM}ms        | ${MS_RSYNC_WARM}ms          | -       |

### Notes

- lunar ws fork: O(1) SQLite INSERT regardless of repo size. Overhead is DB row creation,
  not data copy. No content is duplicated.
- git worktree add: also O(1) (updates a ref pointer). Comparable to lunar fork for new repos.
- git clone: copies all objects. LunarFS fork speedup widens with repo size.
- lunar push deduplicates: second push of same content transfers 0 bytes.
- lunar pull warm: only fetches the root ref (1 HTTP round trip), then skips all blobs
  already in the local CAS.
- rsync cold baseline is a local filesystem copy (no network overhead), so it is an
  optimistic lower bound for any network sync tool.
EOF
