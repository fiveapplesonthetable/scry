# Source this from /mnt/agent/scry to activate scry's Rust toolchain.
# Usage: cd /mnt/agent/scry && . ./env.sh
export RUSTUP_HOME=/mnt/agent/rustup
export CARGO_HOME=/mnt/agent/cargo
export PATH="/mnt/agent/cargo/bin:$PATH"
export SCRY_INDEX_DIR="${SCRY_INDEX_DIR:-/mnt/agent/scry-index}"
