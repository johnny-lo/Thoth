#!/bin/sh
# Compile-checks the Windows half of thoth from a Linux machine:
#   cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings
#
# This proves only that the `#[cfg(windows)]` code still compiles and is
# lint-clean. It runs nothing and links nothing, so it says nothing about
# Windows *behaviour*: run the full `cargo test` on a Windows machine for that
# (docs/post-phase2/linux-support-scope-2026-09-26.md §1).
#
# One-time setup: rustup target add x86_64-pc-windows-gnu
#
# Why the stand-in C compiler: rusqlite's bundled SQLite is C, and its build
# script insists on compiling it for the target even for `check`/`clippy`,
# which never link it. Installing mingw-w64 would need root, so a stand-in
# that claims to be gcc (for cc-rs's compiler-family probe) and writes empty
# objects is enough to get past the build script.
set -eu

here=$(cd "$(dirname "$0")/.." && pwd)
stub_dir="$here/target/windows-check-stub"
mkdir -p "$stub_dir"
cat > "$stub_dir/cc" <<'EOF'
#!/bin/sh
for a in "$@"; do
  [ "$a" = "-E" ] && { echo '#pragma message "gcc"'; exit 0; }
  [ "$a" = "-?" ] && exit 1
done
out=""
while [ $# -gt 0 ]; do
  case "$1" in
    -o) out="$2"; shift 2;;
    -o*) out="${1#-o}"; shift;;
    *) shift;;
  esac
done
[ -n "$out" ] && : > "$out"
exit 0
EOF
chmod +x "$stub_dir/cc"

cd "$here"
CC_x86_64_pc_windows_gnu="$stub_dir/cc" AR_x86_64_pc_windows_gnu=ar \
  cargo clippy --target x86_64-pc-windows-gnu --all-targets -- -D warnings "$@"
