#!/bin/sh
# THOTH_INSTALL_VERSION: vX.Y.Z release tag; unset selects latest.
# THOTH_INSTALL_DIR: destination directory; default $HOME/.local/bin.
# THOTH_INSTALL_NO_SETUP: 1 skips guided setup.
# THOTH_INSTALL_BASE_URL: releases URL or local release-layout directory.
# Do not set THOTH_LOGIN_SKIP_TTY_CHECK; setup needs a real terminal.
# Offline: put the asset and SHA256SUMS in RELEASE/latest/download, then run
# THOTH_INSTALL_BASE_URL=./RELEASE THOTH_INSTALL_DIR=./BIN THOTH_INSTALL_NO_SETUP=1 sh install.sh
# 實測待確認: https://raw.githubusercontent.com/johnny-lo/Thoth/master/install.sh returns 200, UTF-8 without BOM; check curl -fsSI and bytes; fix root placement/README branch URL if needed (also check install.ps1).
# 實測待確認: latest/download redirects work (including Invoke-WebRequest); test isolated installs on Windows and WSL; fallback: API tag_name then download/<tag>.
# 實測待確認: curl | sh setup reads /dev/tty in WSL; fallback: print thoth setup for manual execution.
# 實測待確認: PowerShell 5.1 irm | iex completes and adds user PATH; test isolated directory, new terminal, then remove PATH entry; fix reported 5.1 syntax errors.
set -eu

unsupported() {
    echo 'Unsupported platform; use cargo install --git https://github.com/johnny-lo/Thoth --locked --bin thoth' >&2
    exit 1
}
[ "$(uname -s)" = Linux ] || unsupported
[ "$(uname -m)" = x86_64 ] || unsupported

base=${THOTH_INSTALL_BASE_URL:-https://github.com/johnny-lo/Thoth/releases}
version=${THOTH_INSTALL_VERSION:-}
if [ -n "$version" ]; then
    # A tag cannot introduce path components into either download source.
    case "$version" in *[!v0-9.]*) echo 'Expected THOTH_INSTALL_VERSION=vX.Y.Z' >&2; exit 1 ;; esac
    printf '%s\n' "$version" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$' || {
        echo 'Expected THOTH_INSTALL_VERSION=vX.Y.Z' >&2
        exit 1
    }
    release=download/$version
else
    release=latest/download
fi
command -v curl >/dev/null 2>&1 || { echo 'Requires curl' >&2; exit 1; }
command -v sha256sum >/dev/null 2>&1 || { echo 'Requires sha256sum' >&2; exit 1; }
asset=thoth-x86_64-unknown-linux-gnu
case "$base" in
    http://*|https://*) remote=1 ;;
    *) remote=0; case "$base" in /*) ;; *) base=$PWD/$base ;; esac ;;
esac
tmp=$(mktemp -d "${TMPDIR:-/tmp}/thoth-install-XXXXXXXX")
stage=
cleanup() {
    rm -rf "$tmp"
    if [ -n "$stage" ]; then rm -f "$stage"; fi
}
trap cleanup 0
trap 'exit 1' HUP INT TERM
fetch() {
    if [ "$remote" = 1 ]; then
        curl -fsSL "${base%/}/$release/$1" -o "$tmp/$1"
    else
        cp "$base/$release/$1" "$tmp/$1"
    fi
}
fetch SHA256SUMS
fetch "$asset"
expected=$(awk -v asset="$asset" '
    { sub(/\r$/, "") }
    length($1) == 64 && $1 !~ /[^0-9a-fA-F]/ && ($2 == asset || $2 == "*" asset) && NF == 2 { print tolower($1); count++ }
    END { if (count != 1) exit 1 }
' "$tmp/SHA256SUMS") || { echo "Missing or invalid SHA256SUMS entry for $asset" >&2; exit 1; }
actual=$(sha256sum <"$tmp/$asset")
actual=${actual%% *}
if [ "$expected" != "$actual" ]; then
    echo "SHA256 mismatch: expected $expected; actual $actual" >&2
    exit 1
fi

dir=${THOTH_INSTALL_DIR:-$HOME/.local/bin}
case "$dir" in /*) ;; *) dir=$PWD/$dir ;; esac
mkdir -p "$dir"
stage=$(mktemp "$dir/.thoth-install-XXXXXXXX")
cp "$tmp/$asset" "$stage"
chmod 755 "$stage"
mv -f "$stage" "$dir/thoth"
stage=
"$dir/thoth" --version
case ":$PATH:" in
    *":$dir:"*) ;;
    *)
        case "${SHELL:-}" in */bash) rc='~/.bashrc' ;; */zsh) rc='~/.zshrc' ;; *) rc='~/.profile' ;; esac
        if [ "$dir" = "$HOME/.local/bin" ]; then
            # Print a literal command for the user's next shell.
            # shellcheck disable=SC2016
            echo 'Add to PATH: export PATH="$HOME/.local/bin:$PATH"'
        else
            # Escape single quotes so a custom path is safe to paste into sh.
            quoted=$(printf '%s' "$dir" | sed "s/'/'\\\\''/g")
            printf "Add to PATH: export PATH='%s':\"\$PATH\"\n" "$quoted"
        fi
        echo "Put this line in $rc."
        ;;
esac
manual_setup() {
    echo 'Run later: thoth setup'
    if [ -n "${THOTH_HOME:-}" ]; then
        printf 'Keep THOTH_HOME set to %s when running thoth setup.\n' "$THOTH_HOME"
    fi
}
if [ "${THOTH_INSTALL_NO_SETUP:-}" = 1 ]; then
    manual_setup
elif [ -t 1 ]; then
    if [ -t 0 ]; then
        if "$dir/thoth" setup; then :; else echo 'Setup still has failing items; rerun thoth setup'; fi
    elif sh -c ': </dev/tty' 2>/dev/null; then
        if "$dir/thoth" setup </dev/tty; then :; else echo 'Setup still has failing items; rerun thoth setup'; fi
    else
        manual_setup
    fi
else
    manual_setup
fi
