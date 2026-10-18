# Distributing Hearth

The source checkout is the portable installation path across Linux distributions.
Users build with Cargo and install with `scripts/install.sh`. No service manager
is required. The project is not currently published on crates.io.

## Binary bundles

From a source checkout with Python 3 and Rust installed:

```sh
python3 scripts/package.py
```

This builds the host target and writes a versioned `.tar.gz` plus a `.sha256`
checksum under `dist/`. The bundle contains all three executables, installation
script, README, service examples, agent skill, and any project license files.
Runtime databases, credentials, mounts, logs, and development artifacts are not
included. It creates local files only; it does not publish a GitHub release.

For a different target, use `--target TARGET` after installing that Rust target
and any required cross-compilation toolchain. Architecture and libc are part of
the archive name: a `*-unknown-linux-gnu` build requires compatible glibc and
is not a universal Linux binary. Build on the oldest distribution you intend to
support, test on the target distributions, or build/test a musl target. The FUSE
client still needs `/dev/fuse` and `fusermount3` at runtime.

Extract a bundle, enter its directory, then install without Rust:

```sh
sh scripts/install.sh --prefix "$HOME/.local"
```

Checksums can be verified with `sha256sum -c ARCHIVE.tar.gz.sha256` from the
download directory. The installer also accepts `--client-only`, `--bin-dir PATH`,
and `--destdir /staging` for packaging. `--destdir` stages files beneath the chosen
prefix without starting services or changing accounts or configuration.

## GitHub checks

`.github/workflows/ci.yml` runs Rust formatting, Clippy, unit tests, IRC protocol
tests, real FUSE tests, installation tests, and systemd unit validation on Ubuntu.
It also checks compilation with the minimum supported Rust version. The FUSE
job expects a Linux VM runner with `/dev/fuse`; a container without FUSE device
access is not an equivalent test environment.

Hearth IRC uses the [WTFPL, version 2](../LICENSE); the installer and release
bundle include its license text.

Before a public release, test the release bundle on the platforms you advertise.
Do not upload your entire working directory: use a
clean source checkout or the bundle script.
