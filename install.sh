#!/bin/sh
# Install jockey on macOS: the Xcode command line tools and Rust if they're
# missing, then the `jockey` binary into ~/.cargo/bin. Run it from a checkout,
# or straight from GitHub:
#   curl -fsSL https://raw.githubusercontent.com/lorenzopi/jockey/main/install.sh | sh
set -eu

[ "$(uname)" = Darwin ] || { echo "install.sh is for macOS. Elsewhere: cargo install --locked --git https://github.com/lorenzopi/jockey" >&2; exit 1; }

if ! xcode-select -p >/dev/null 2>&1; then
    echo "jockey needs the Xcode command line tools (for the linker). Opening the installer;"
    echo "run this installer again when it finishes."
    xcode-select --install
    exit 1
fi

PATH="$HOME/.cargo/bin:$PATH"
if ! command -v cargo >/dev/null 2>&1; then
    echo "Installing Rust with rustup (https://rustup.rs)..."
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
fi

# Edition 2024 needs Rust 1.85 or newer.
minor=$(rustc --version | cut -d' ' -f2 | cut -d. -f2)
if [ "$minor" -lt 85 ]; then
    if command -v rustup >/dev/null 2>&1; then
        rustup update stable
    else
        echo "jockey needs Rust 1.85+, found $(rustc --version). Update Rust and run this again." >&2
        exit 1
    fi
fi

# From a checkout, install it; piped from curl, fetch the latest from GitHub.
dir=$(dirname "$0")
if grep -qs '^name = "jockey"' "$dir/Cargo.toml"; then
    cargo install --locked --path "$dir"
else
    cargo install --locked --git https://github.com/lorenzopi/jockey
fi

echo
echo "Installed $(jockey --version 2>/dev/null || echo jockey) to ~/.cargo/bin"
case ":$(printenv PATH):" in
    *":$HOME/.cargo/bin:"*) ;;
    *) echo "Add ~/.cargo/bin to your PATH (open a new terminal if rustup was just installed)." ;;
esac
found=""
for cli in codex claude grok; do
    command -v "$cli" >/dev/null 2>&1 && found="$found $cli"
done
if [ -n "$found" ]; then
    echo "Agent CLIs found:$found. Run \`jockey doctor\`, then \`jockey\` in a project."
else
    echo "No agent CLI found. Install and sign in to at least one of codex, claude or grok,"
    echo "or try the UI with \`jockey --agent mock\`."
fi
