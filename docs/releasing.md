# Linux releases

GP200 Linux is an independent, MIT-licensed Linux editor for the Valeton GP-200.
The public release workflow produces preview packages; no hardware acceptance is
implied by a successful CI run. It creates a **draft** GitHub release for a
maintainer to inspect and publish.

## Supported package baseline

Release binaries are built on Ubuntu 24.04 for `x86_64-unknown-linux-gnu`, with
GCC/G++ 13 and the Rust version in `rust-toolchain.toml`. The declared runtime
baseline is Linux x86_64 with glibc 2.39, ALSA (`libasound.so.2`), the Ubuntu 24.04
C++ runtime or newer, Mesa/OpenGL, and a Wayland or X11 desktop. File dialogs use
a running D-Bus session, `xdg-desktop-portal` and the desktop's portal backend.
The package records the actual GUI, CLI and embedded calibration-helper ELF
requirements in `release-manifest.json` and refuses a build requiring a newer
glibc or C++ symbol version than the baseline (`GLIBCXX_3.4.32`,
`CXXABI_1.3.14`, the [GCC 13.2 ABI](https://gcc.gnu.org/onlinedocs/libstdc++/manual/abi.html)). Older distributions and other architectures are not
supported package targets yet.

The AppImage bundles selected direct libraries and the X11 support libraries loaded at runtime. Graphics drivers, glibc, ALSA,
the C++ runtime, desktop services and MIDI permissions remain host dependencies.
The embedded offline NAM calibration helper runs without runtime downloads.
Its exact hash and required `GLIBCXX`/`CXXABI` versions are recorded in the
manifest. A compiler's C++20 mode alone does not establish old-distribution
compatibility; the embedded helper must meet the same runtime baseline.

## Build and verify locally

Use an Ubuntu 24.04 x86_64 machine or equivalent container. Install build and
packaging dependencies:

```sh
sudo apt-get update
sudo apt-get install --no-install-recommends -y build-essential gcc-13 g++-13 pkg-config libasound2-dev libdbus-1-dev libudev-dev libx11-dev libxkbcommon-dev libwayland-dev libegl1-mesa-dev libgl1-mesa-dev xvfb xauth desktop-file-utils libxcursor1 libxi6 libxrandr2 libxinerama1 libgl1 libegl1 libglx-mesa0 libgl1-mesa-dri dbus libxkbcommon-x11-0 libxcb-cursor0 squashfs-tools patchelf
rustup show active-toolchain
export CC=gcc-13 CXX=g++-13 RUST_TEST_THREADS=4
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --all-targets --locked
cargo clippy --workspace --all-targets --locked --no-default-features -- -D warnings
cargo test --workspace --all-targets --locked --no-default-features
python3 -m unittest discover -s packaging -p 'test_*.py'
python3 packaging/firmware-resources-tests.py
export SOURCE_DATE_EPOCH="$(git show -s --format=%ct HEAD)"
python3 packaging/build-linux-release.py --output target/dist
python3 packaging/smoke-portable.py target/dist/GP200-Linux-*-x86_64.tar.gz --output target/portable-smoke
python3 packaging/smoke-native.py target/dist/GP200-Linux-*-x86_64.AppImage --appimage --output target/appimage-smoke
```

Rustup must already be installed; its activation installs the exact repository
toolchain if needed. Use a fresh output directory when rebuilding. Build scripts
do not publish or overwrite release assets. The smoke checks isolate config and
data directories, run the real native renderer under Xvfb, check a simulator
screenshot, and exercise CLI/help and portable install/upgrade/uninstall. They
do not access a pedal or validate firmware transfer.

For only a portable archive, run `python3 packaging/build-portable.py`. Both
builders compile the GUI with `firmware-linux-completion`, alongside the CLI,
with `Cargo.lock` enforced. The standalone portable builder can also package
explicit existing `--binary` and `--cli-binary` paths; its manifest labels the
source correspondence as unverified. Use the full release builder for release
assets.

## Prepare a release draft

1. Update the workspace version in `Cargo.toml`, regenerate `Cargo.lock` if
   needed, and commit the reviewed source with a Conventional Commit message.
2. Confirm CI passes on a clean public checkout. Review documentation, included
   third-party notices and the hardware scope stated in the release notes.
3. Tag that commit as `v<workspace version>` and push the tag using local Git.
   For the initial version this is `v0.1.0`; prereleases such as
   `v0.2.0-rc.1` must match the workspace version exactly.
4. The tag workflow repeats the CI checks, builds and smokes both packages,
   verifies `SHA256SUMS`, and attaches these assets to a draft prerelease:
   `GP200-Linux-<version>-x86_64.AppImage`,
   `GP200-Linux-<version>-x86_64.tar.gz`, and `SHA256SUMS`.
5. Inspect the draft's artifacts and smoke screenshots, test on the supported
   desktop, and replace the generated notes with the actual changes and known
   limitations before publishing. A stable release requires an explicit
   maintainer decision; the workflow never marks a release stable or latest.

The workflow requires GitHub Actions and the repository's `GITHUB_TOKEN` to
have permission to create draft releases. A rerun refuses to overwrite assets
already attached to the same draft; inspect any partially uploaded draft before
retrying. This repository's agent tooling accesses GitHub through the GitHub
plugin. Ordinary local Git remains available for commits, tags, fetches and
pushes.

## Release inputs and notices

GitHub Actions are pinned to commit SHAs. `packaging/appimage-tools.json` pins
linuxdeploy, appimagetool and the type-2 AppImage runtime to named releases and
SHA-256 digests. The builder validates every downloaded tool before execution
and passes `--runtime-file` to appimagetool, preventing its default download of a
moving runtime. No remote shell script is executed. Update pins deliberately
after checking their release metadata through the GitHub plugin and rerunning
the package smoke checks; do not replace them with `latest` or `continuous`.

Packages retain the MIT application license, locked Rust dependency notices,
Cantarell's font notice, and NAM/NamtoClo/r8brain/Ooura notices. The AppImage also
retains system-library copyright files copied by linuxdeploy, full common
license texts and AppImage runtime notices. Its documentation includes the
runtime source at the locked commit and the checked libfuse 3.15.0 source archive;
the runtime tree contains the libfuse patch and build/relink instructions. The
runtime is a separate executable prefix, so it can be replaced without changing
the packaged application's source or data. The outer AppImage manifest records
the adjusted ELF and bundled-library hashes; the portable manifest records the
original compiled executable hashes. A normalized tar archive timestamp makes
the archive metadata deterministic; this is not a claim of byte-identical Rust
or AppImage builds across different build hosts.

Public packages use a fixed file allowlist. Vendor installers, official firmware
images, private captures, private investigation notes and acceptance receipts
are excluded. Public firmware protocol/support resources are embedded into the
GUI and `docs/firmware.md` is included. The old internal name `gp200-studio` remains in launcher, desktop
IDs, configuration and data paths for compatibility. The display and asset names
are GP200 Linux.

## Firmware scope

Private development testing exercised a **GP-200 V1.6.3 → V1.8.2** upgrade, with
full-image recovery after an interrupted attempt. That is the tested path. It
does not establish support for other starting firmware, other target images,
GP-200LT or other hardware, nor does it validate newly built public package
bytes. Private source-bound receipts and raw firmware are not public release
assets.

The GUI's default build includes the `firmware-linux-completion` updater and
public protocol/support resources. When the user explicitly prepares an update,
the app materializes its embedded support files in a private local data
directory and binds the resource manifest to the executable that is actually
running. This also works after AppImage ELF relocation. No separately downloaded
configuration bundle is required. The user supplies official firmware and
reviews the app's warnings, identity checks and write approval before transfer;
starting the editor never flashes automatically. See [firmware.md](firmware.md)
for the supported workflow and limits.

`GP200_FIRMWARE_RESOURCES` remains a strict development override. Do not remove
resource admission, identity binding or approval checks to bypass an unsupported
image or device. The public resource bootstrap has not been tested on hardware
in this publication preparation; the historical V1.6.3 → V1.8.2 development
observation above remains the evidence boundary.
