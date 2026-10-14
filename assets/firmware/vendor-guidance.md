# Valeton GP-200 firmware guidance

Summary of the [official GP-200 FAQ](https://www.valeton.net/gp-200-faq/), checked
14 October 2026:

Valeton explains that entering the update process requires completing an update
to return to normal. If interrupted updating leaves the update/restore screen,
its guidance is to restore suitable operating conditions and run the update
again. This statement does not specify Linux packet retries or journal resume.

The FAQ permits direct upgrades across intermediate versions and says to use
the compatible editor version recommended for the firmware. Check the firmware's
release notes. Prefer a direct computer USB connection to reduce interference
from other devices sharing a hub. Maintain stable power and USB throughout an
attempt.

Read the application firmware version on the pedal under GLOBAL SETTINGS →
About → Firmware Version. Bootloader identity and the selected image label
cannot substitute for this observation.

GP200 Linux is independent software. This summary does not claim vendor support
for Linux or approve a device operation. The app's separate current capture,
query, transfer and completion checks apply to every attempt.
