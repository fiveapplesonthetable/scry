#!/usr/bin/env bash
# install_indexers.sh — one-shot install of every indexer scry's
# build-symbol precision pipeline consumes.
#
# Usage:
#   bash <(curl -fsSL https://raw.githubusercontent.com/fiveapplesonthetable/scry/master/scripts/install_indexers.sh)
# or, from a checkout:
#   ./scripts/install_indexers.sh
#
# What this installs (idempotent — re-running is a no-op for what's
# already present):
#
#   System packages (uses your distro's package manager):
#     - libclang-18-dev  (Path B: C / C++ / ObjC USRs)
#     - clang clang++    (the actual compiler libclang wraps)
#     - default-jdk      (needed by scip-java + scip-kotlin)
#     - golang-go        (needed by scip-go)
#     - npm + nodejs     (needed by scip-typescript, scip-python)
#
#   Per-language indexer binaries → ${PREFIX}/bin (default ~/.local/bin):
#     - scip-typescript   (npm global)
#     - scip-python       (npm global)
#     - rust-analyzer     (rustup component)
#     - scip-go           (go install github.com/scip-code/scip-go)
#     - scip-java         (GitHub release tarball; needs gradle on PATH
#                          only if you point it at a Gradle project)
#
# AOSP Java/Kotlin/C++ precision uses the Kythe kzip pipeline
# instead of per-language SCIP indexers — see docs/PIPELINE.md
# and `scry build-symbols --build-kzip PATH`.
#
# After this script finishes:
#   scry index <ROOT> -o <IDX>
#   <generate compile_commands.json or *.scip per-project>
#   scry finalize --index <IDX> --build-out <project>
#   scry callers Foo --index <IDX>      # auto-engages USR / SCIP narrowing

set -euo pipefail

PREFIX="${PREFIX:-$HOME/.local}"
BUILD_DIR="${SCRY_BUILD_DIR:-$HOME/scry-build}"
mkdir -p "$PREFIX/bin" "$BUILD_DIR"

# Make $PREFIX/bin visible to any subprocesses we launch.
export PATH="$PREFIX/bin:$PATH"

log()  { printf "\033[36m[install]\033[0m %s\n" "$*"; }
warn() { printf "\033[33m[install] WARN:\033[0m %s\n" "$*"; }
have() { command -v "$1" >/dev/null 2>&1; }

# ---------------------------------------------------------------------------
# 1. System packages
# ---------------------------------------------------------------------------
install_system_pkgs() {
    if have apt-get; then
        log "installing system packages via apt-get (you may be prompted for sudo)"
        sudo apt-get update -qq
        sudo apt-get install -y --no-install-recommends \
            clang libclang-18-dev \
            default-jdk \
            golang-go \
            nodejs npm \
            curl unzip git
    elif have dnf; then
        sudo dnf install -y clang clang-devel java-21-openjdk-devel golang nodejs npm curl unzip git
    elif have brew; then
        brew install llvm openjdk go node curl unzip git
    else
        warn "no supported package manager found (apt-get / dnf / brew)."
        warn "install manually: clang, libclang-dev, JDK, go, npm, node, curl, unzip, git."
    fi
}

# ---------------------------------------------------------------------------
# 2. npm-based indexers: scip-typescript, scip-python
# ---------------------------------------------------------------------------
install_npm_indexers() {
    if ! have npm; then warn "npm absent — skipping scip-typescript / scip-python"; return; fi
    log "installing scip-typescript + scip-python (npm prefix=$PREFIX)"
    npm install --silent --prefix "$PREFIX" \
        @sourcegraph/scip-typescript \
        @sourcegraph/scip-python
    # npm --prefix installs binaries to $PREFIX/lib/node_modules/.bin;
    # symlink them under $PREFIX/bin so they're on PATH.
    for b in scip-typescript scip-python; do
        local src="$PREFIX/lib/node_modules/.bin/$b"
        if [[ -e "$src" ]]; then ln -sf "$src" "$PREFIX/bin/$b"; fi
    done
}

# ---------------------------------------------------------------------------
# 3. rust-analyzer (via rustup component)
# ---------------------------------------------------------------------------
install_rust_analyzer() {
    if ! have rustup; then
        warn "rustup absent — install rustup first to enable Rust SCIP"
        return
    fi
    log "adding rust-analyzer rustup component"
    rustup component add rust-analyzer
    # rustup proxy lives at ~/.cargo/bin/rust-analyzer; PATH should
    # already cover ~/.cargo/bin if you installed Rust via rustup.
}

# ---------------------------------------------------------------------------
# 4. scip-go (Go module install)
# ---------------------------------------------------------------------------
install_scip_go() {
    if ! have go; then warn "go absent — skipping scip-go"; return; fi
    log "installing scip-go (GOBIN=$PREFIX/bin)"
    GOBIN="$PREFIX/bin" go install github.com/scip-code/scip-go/cmd/scip-go@latest
}

# ---------------------------------------------------------------------------
# 5. scip-java (GitHub release launcher script)
# ---------------------------------------------------------------------------
install_scip_java() {
    if have scip-java; then log "scip-java already on PATH"; return; fi
    log "installing scip-java v0.12.3"
    curl -fsSL https://github.com/sourcegraph/scip-java/releases/download/v0.12.3/scip-java-v0.12.3 \
        -o "$PREFIX/bin/scip-java"
    chmod +x "$PREFIX/bin/scip-java"
}

# ---------------------------------------------------------------------------
# Wrap-up: print a one-line status summary so the user knows what's wired.
# ---------------------------------------------------------------------------
summary() {
    log "---- indexer install summary ----"
    for tool in scip-typescript scip-python rust-analyzer scip-go scip-java; do
        if have "$tool"; then printf "  OK   %s -> %s\n" "$tool" "$(command -v "$tool")"
        else printf "  MISS %s (not installed)\n" "$tool"
        fi
    done
    log "PATH still needs \$PREFIX/bin? Add to your shell rc:"
    echo "    export PATH=\"$PREFIX/bin:\$PATH\""
}

install_system_pkgs
install_npm_indexers
install_rust_analyzer
install_scip_go
install_scip_java
summary
