# GP200 Linux — Linux x86_64 preview

Start `./bin/gp200-studio`. `./bin/gp200-studio --demo` opens the labelled
simulator. Connecting to a pedal requires an explicit action. For X11/XWayland
use `./bin/gp200-studio --x11`. `./bin/gp200-studio --help` lists GUI options;
`./bin/gp200-studio cli --help` lists the headless CLI commands.

Packages support the Ubuntu 24.04 x86_64 baseline: glibc 2.39, ALSA,
the Ubuntu 24.04 C++ runtime or newer, Mesa/OpenGL, and a Wayland or X11 desktop.
Native file dialogs use D-Bus and `xdg-desktop-portal` with your desktop's portal
backend. Exact executable/helper requirements are in `release-manifest.json`.
The embedded NAM calibration helper performs no runtime downloads.

Optionally install as your ordinary account with `python3 install-user.py`
(default `~/.local`) or `python3 install-user.py --prefix /absolute/path`.
The installer adds `bin/gp200-studio`, a desktop entry and file associations;
it does not change default file associations. Repeating the command upgrades
files owned by this installer and refuses to overwrite modified or foreign
files. Close other app windows before upgrading. Uninstall with
`python3 uninstall-user.py`, using the same `--prefix`. Personal data remains.

Configuration and ordinary app data retain the compatibility name
`gp200-studio`: `$XDG_CONFIG_HOME/gp200-studio` and
`$XDG_DATA_HOME/gp200-studio` (normally `~/.config` and `~/.local/share`).
No root process, setuid binary or broad USB permission rule is installed.
Use your account's ALSA MIDI access and close competing editors before
connecting. After an uncertain device write, reconnect and read the device;
do not blindly repeat an unconfirmed save.

The updater and public support resources are included. Official firmware images
must be supplied by the user; transfer requires explicit review, the correct
device identity and approval. No update starts automatically. Read
`docs/firmware.md` before using update or recovery. Historical private testing
covered GP-200 V1.6.3 → V1.8.2, including full-image recovery after an interrupted
attempt. These public package bytes have not been validated on a pedal and that
path does not establish support for other firmware or hardware.

See `docs/releasing.md` for build inputs and the package scope. Application,
dependency, font and native-helper notices are in `licenses/`;
`dependency-licenses.json` records the locked Cargo graph. Vendor installers,
official firmware and private acceptance evidence are excluded.
