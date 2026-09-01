# Installation

mcpgw is one static binary with no runtime dependencies. Pick whichever of
these you already trust.

## Installer script

```sh
curl -fsSL https://github.com/kennywillbe/mcpgw/releases/latest/download/mcpgw-installer.sh | sh
```

Detects your platform, downloads that archive and puts the binary in
`~/.local/bin`. Two knobs:

```sh
MCPGW_INSTALL_DIR=/usr/local/bin sh -c "$(curl -fsSL .../mcpgw-installer.sh)"
MCPGW_VERSION=0.1.0              sh -c "$(curl -fsSL .../mcpgw-installer.sh)"
```

The script does not check signatures or hashes. If you want that, use the
archive route below and verify against `SHA256SUMS` yourself.

## Homebrew

```sh
brew install kennywillbe/tap/mcpgw
```

## Cargo

```sh
cargo install mcpgw
```

Builds from source, so it works on any target Rust supports — including ones
with no prebuilt archive.

## From a release archive

Every release attaches one archive per platform plus a combined `SHA256SUMS`:

```sh
version=0.1.0
target=aarch64-apple-darwin
base=https://github.com/kennywillbe/mcpgw/releases/download/v${version}

curl -fsSLO "${base}/mcpgw-${version}-${target}.tar.gz"
curl -fsSLO "${base}/SHA256SUMS"
grep " mcpgw-${version}-${target}.tar.gz\$" SHA256SUMS | shasum -a 256 -c -

tar -xzf "mcpgw-${version}-${target}.tar.gz"
install "mcpgw-${version}-${target}/mcpgw" /usr/local/bin/
```

Prebuilt targets: `aarch64-apple-darwin`, `x86_64-apple-darwin`,
`x86_64-unknown-linux-gnu`, `x86_64-pc-windows-msvc`.

## Check it

```sh
mcpgw --version
mcpgw doctor
```

On a fresh machine `doctor` will tell you there's no config yet and list which
clients it found. That's the expected first-run state — go to the
[Quickstart](./quickstart.md).
