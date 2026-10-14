# Firmware updating on Linux

Ordinary Linux builds include the firmware updater. Open Assets → Firmware and
select your own official **GP-200 V1.8.2** image. Firmware is not bundled or
automatically downloaded. The supported image is 6,452,728 bytes with SHA-256
`c368618773097b07c556c951eb10792f9d57be929323366648eb6e083d0d23af`.
Other images, GP-200LT and GP-200JR are rejected by this route.

Review the image, current device capture and backup/loss coverage before entering
update mode. Entry, the fresh boot query and transfer each require their own
current confirmation in the app. Transfer follows selectors `00/01/02/10` and
raw image-table slots `0/2/3/1`. The app offers each action once, applies finite
deadlines and retains durable history; it does not automatically retry or resume.

After transfer, the app requires fresh normal reads and your new observation of
**GLOBAL SETTINGS → About → Firmware Version = V1.8.2**. Both `V1.8.2` and
`v1.8.2` are accepted. Firmware completion does not establish preservation or
restoration of presets, metadata or installed IR audio. Installed IR audio
cannot be backed up through this workflow; keep your original files.

If an attempt stops, preserve the original local library and use **History &
recovery** for a separately reviewed full-image attempt from section/block zero.
Existing approvals and journals never authorize resumption. Failed or incomplete
completion keeps recovery held and ordinary device editing blocked.

The app contains its public protocol description and review instructions. On
explicit updater preparation it creates private support files beneath
`$XDG_DATA_HOME/gp200-linux/firmware-resources`, falling back to
`$HOME/.local/share/gp200-linux/firmware-resources`. Support directories use mode
0700 and files use 0600. Their version/content hash and the actual running
executable are pinned and revalidated by the live review. No source checkout,
adjacent JSON file or private development evidence is required. Existing library
and preferences continue to use the `gp200-studio` namespace.

Developers may explicitly set `GP200_FIRMWARE_RESOURCES` to a schema-1,
executable-bound resource manifest. The strict external loader and existing
live admission checks still apply. Ordinary launches use compiled resources.

See the [backup/loss review](../assets/firmware/loss.md),
[Linux recovery instructions](../assets/firmware/recovery.md) and
[official guidance summary](../assets/firmware/vendor-guidance.md).
