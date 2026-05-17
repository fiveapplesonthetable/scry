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
#     - scip-java         (GitHub release tarball)
#     - gradle 8.10.2     (Apache distribution; needed by scip-java/kotlin)
#
#   Built-from-source (~/scry-build/scip-kotlin):
#     - sbt 1.10.5        (downloaded; needed to build scip-kotlin)
#     - semanticdb-kotlinc  (sbt publishM2 → ~/.m2/repository)
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
# 6. Gradle (needed by scip-java + scip-kotlin paths)
# ---------------------------------------------------------------------------
install_gradle() {
    if have gradle; then log "gradle already on PATH"; return; fi
    log "installing gradle 8.10.2"
    local zip="$BUILD_DIR/gradle-8.10.2-bin.zip"
    curl -fsSL https://services.gradle.org/distributions/gradle-8.10.2-bin.zip -o "$zip"
    unzip -q -o "$zip" -d "$BUILD_DIR"
    ln -sf "$BUILD_DIR/gradle-8.10.2/bin/gradle" "$PREFIX/bin/gradle"
}

# ---------------------------------------------------------------------------
# 7. sbt + scip-kotlin (semanticdb-kotlinc, built from source)
# ---------------------------------------------------------------------------
install_sbt_and_scip_kotlin() {
    if ! have sbt; then
        log "installing sbt 1.10.5"
        local sbt_tgz="$BUILD_DIR/sbt.tgz"
        curl -fsSL https://github.com/sbt/sbt/releases/download/v1.10.5/sbt-1.10.5.tgz \
            -o "$sbt_tgz"
        tar -xzf "$sbt_tgz" -C "$BUILD_DIR"
        ln -sf "$BUILD_DIR/sbt/bin/sbt" "$PREFIX/bin/sbt"
    fi
    if [[ -d "$HOME/.m2/repository/com/sourcegraph/semanticdb-kotlinc" ]]; then
        log "semanticdb-kotlinc already published to ~/.m2"
        return
    fi
    log "cloning scip-kotlin + sbt publishM2 (this builds the Kotlin compiler plugin)"
    local kt_src="$BUILD_DIR/scip-kotlin-src"
    if [[ ! -d "$kt_src" ]]; then
        git clone --depth 1 https://github.com/sourcegraph/scip-kotlin.git "$kt_src"
    fi
    # Patch: silence the spurious "unknown symbol kind FirFileSymbol"
    # stderr noise upstream emits on Kotlin 2.x. The plugin already
    # returned NONE for that case — it just printed first, which made
    # scry-bridge mis-classify every kotlinc compilation containing
    # a top-level fun/val as "partial / no output". The fix is one
    # explicit `is FirFileSymbol -> SemanticdbSymbolDescriptor.NONE`
    # arm before the catch-all in the `semanticdbDescriptor` cascade.
    local cache_file="$kt_src/semanticdb-kotlinc/src/main/kotlin/com/sourcegraph/semanticdb_kotlinc/SymbolsCache.kt"
    if [[ -f "$cache_file" ]] && ! grep -q "symbol is FirFileSymbol -> SemanticdbSymbolDescriptor.NONE" "$cache_file"; then
        log "applying FirFileSymbol noise-suppression patch to $cache_file"
        # Insert the explicit FirFileSymbol arm before the catch-all `else`
        # that prints "unknown symbol kind …" to stderr.
        python3 -c '
import io, sys, re
path = "'"$cache_file"'"
src = open(path).read()
needle = ("            symbol is FirVariableSymbol ->\n"
          "                SemanticdbSymbolDescriptor(Kind.TERM, symbol.name.toString())\n"
          "            else -> {\n"
          "                err.println(\"unknown symbol kind ${symbol.javaClass.simpleName}\")")
repl   = ("            symbol is FirVariableSymbol ->\n"
          "                SemanticdbSymbolDescriptor(Kind.TERM, symbol.name.toString())\n"
          "            // patched-by-scry-install: FirFileSymbol legitimately has no\n"
          "            // SemanticDB descriptor (file-level container, not a declaration).\n"
          "            // Returning NONE silently here matches the previous catch-all\n"
          "            // behaviour minus the spurious stderr noise.\n"
          "            symbol is FirFileSymbol -> SemanticdbSymbolDescriptor.NONE\n"
          "            else -> {\n"
          "                err.println(\"unknown symbol kind ${symbol.javaClass.simpleName}\")")
if needle not in src:
    sys.exit("patch needle not found; upstream may have refactored — skipping")
open(path, "w").write(src.replace(needle, repl, 1))
' || warn "patch could not be applied (upstream may have changed); proceeding anyway"
    fi
    (cd "$kt_src" && "$PREFIX/bin/sbt" publishM2)
}

# ---------------------------------------------------------------------------
# Wrap-up: print a one-line status summary so the user knows what's wired.
# ---------------------------------------------------------------------------
summary() {
    log "---- indexer install summary ----"
    for tool in scip-typescript scip-python rust-analyzer scip-go scip-java gradle sbt; do
        if have "$tool"; then printf "  \033[32mOK\033[0m   %s -> %s\n" "$tool" "$(command -v "$tool")"
        else printf "  \033[31mMISS\033[0m %s (not installed)\n" "$tool"
        fi
    done
    if [[ -d "$HOME/.m2/repository/com/sourcegraph/semanticdb-kotlinc" ]]; then
        printf "  \033[32mOK\033[0m   semanticdb-kotlinc (~/.m2/repository/com/sourcegraph/semanticdb-kotlinc)\n"
    else
        printf "  \033[31mMISS\033[0m semanticdb-kotlinc (~/.m2)\n"
    fi
    log "PATH still needs \$PREFIX/bin? Add to your shell rc:"
    echo "    export PATH=\"$PREFIX/bin:\$PATH\""
}

install_system_pkgs
install_npm_indexers
install_rust_analyzer
install_scip_go
install_scip_java
install_gradle
install_sbt_and_scip_kotlin
summary
