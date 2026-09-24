REPO_DIR := justfile_directory()
LOCAL_BIN := "/tmp/insomnia-out/insomnia"

_default:
    @just --list

# ─── Deploy (docker build + local binary + VPS image build) ─────────────
#
# build-vps / restart are dual-side recipes:
#
#   with HOST              → local driver: compile release binary in docker
#                            (rust:1-bookworm — same glibc as the runtime
#                            image), rsync it, then run the VPS half over ssh
#   without HOST (on VPS)  → the VPS half that does the actual work
#
#   just deploy root@server.com
#
# One-time VPS setup:
#   git clone https://github.com/acidnik/insomnia ~/insomnia
#   mkdir -p ~/insomnia/checks ~/insomnia/state
#   cp deploy/config.vps.example.toml ~/insomnia/config.toml  # fill secrets
#
# libexec/ is baked into the image; config.toml, checks/ and state/ are
# bind-mounted from ~/insomnia on the host (see deploy/docker-compose.vps.yml).

# Full release pipeline: push HEAD to origin, rebuild and restart on the VPS.
deploy HOST: _check-git-clean
    #!/usr/bin/env bash
    set -euo pipefail
    cd {{justfile_directory()}} && git push origin main
    just build-vps "{{HOST}}"
    just restart "{{HOST}}"

# Rebuild the image on the VPS. Does not touch the running container —
# follow up with `just restart` (or run `just deploy` for the full pipeline).
build-vps HOST="": _check-git-clean
    #!/usr/bin/env bash
    set -euo pipefail

    if [ -n "{{HOST}}" ]; then
        echo "--- Building insomnia release in docker (rust:1-bookworm)..."
        # compile via deploy/Dockerfile.build: a bookworm-based builder, so
        # glibc matches the runtime image (debian:bookworm-slim) — a binary
        # built on the host (e.g. Arch) may not run on the VPS. Cargo caches
        # live in BuildKit cache mounts: incremental, no host pollution.
        # The result lands in /tmp/insomnia-out/, no image is kept.
        cd {{REPO_DIR}} && docker build \
            -f deploy/Dockerfile.build \
            --output type=local,dest=/tmp/insomnia-out \
            .
        test -f {{LOCAL_BIN}} || { echo "no binary at {{LOCAL_BIN}}"; exit 1; }

        echo "--- Copying binary to {{HOST}}..."
        rsync -az -e ssh "{{LOCAL_BIN}}" "{{HOST}}:/tmp/insomnia-bin"

        # Pull the just-pushed code first so the invoked recipe is the fresh one.
        echo "--- Syncing code to {{HOST}}..."
        ssh "{{HOST}}" 'cd ~/insomnia && git pull --ff-only'

        echo "--- Running build-vps on {{HOST}}..."
        ssh "{{HOST}}" 'cd ~/insomnia && just build-vps'
    else
        # VPS half — invoked over SSH by the local driver. Refuse to run
        # on a machine that isn't the VPS.
        test -d ~/insomnia || { echo "run as: just build-vps HOST"; exit 1; }

        cd ~/insomnia
        git pull --ff-only
        git log --oneline -n1

        test -f /tmp/insomnia-bin || { echo "no binary at /tmp/insomnia-bin"; exit 1; }
        # Per-project build context — a shared /tmp/deploy-ctx collides with
        # other projects' deploys on the same VPS.
        CTX=/tmp/insomnia-deploy-ctx
        rm -rf "$CTX"
        mkdir -p "$CTX"
        cp /tmp/insomnia-bin "$CTX"/insomnia
        cp -r ~/insomnia/libexec "$CTX"/libexec
        cp ~/insomnia/deploy/Dockerfile.deploy "$CTX"/
        cd "$CTX" && docker build -t localhost/insomnia -f Dockerfile.deploy .
    fi

# Restart the daemon container on the VPS and wait until it watches checks.
# No rebuild — run `just build-vps` first if the code changed.
restart HOST="":
    #!/usr/bin/env bash
    set -euo pipefail

    if [ -n "{{HOST}}" ]; then
        echo "--- Restarting insomnia on {{HOST}}..."
        ssh "{{HOST}}" 'cd ~/insomnia && just restart'
    else
        # VPS half.
        test -d ~/insomnia || { echo "run as: just restart HOST"; exit 1; }
        cd ~/insomnia
        docker compose -f deploy/docker-compose.vps.yml up -d --force-recreate app

        for i in $(seq 1 20); do
            if docker logs insomnia 2>&1 | grep -q "watching /app/checks"; then
                echo "OK: insomnia is watching /app/checks"
                docker logs --tail=10 insomnia
                exit 0
            fi
            sleep 1
        done
        echo "insomnia failed to start"
        docker logs --tail=50 insomnia
        exit 1
    fi

# ─── Internal helpers ───────────────────────────────────────────────────────

# Verify the working tree is clean and all commits are pushed.
_check-git-clean:
    @echo "Checking git working copy..."
    @cd {{justfile_directory()}} && \
        git diff --quiet --exit-code || (echo "❌ Uncommitted changes"; exit 1)
    @cd {{justfile_directory()}} && \
        git diff --cached --quiet --exit-code || (echo "❌ Staged but uncommitted changes"; exit 1)
    @cd {{justfile_directory()}} && \
        (git rev-parse --abbrev-ref @{u} &>/dev/null) || \
        (echo "❌ no upstream; push first"; exit 1)
    @cd {{justfile_directory()}} && git fetch --quiet 2>/dev/null || true
    @cd {{justfile_directory()}} && \
        ([ "$(git rev-parse HEAD)" = "$(git rev-parse @{u})" ]) || \
        (echo "❌ Local HEAD differs from origin; push first"; exit 1)
    @echo "✓ Git working copy is clean and pushed to origin"
