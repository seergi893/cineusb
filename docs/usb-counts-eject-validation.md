# Recursive series counts and safe ejection

## Causes established from the code

Series counting previously entered only root folders whose names passed the season-name parser. Unrecognized containers were skipped, including every episode below them. Completed scans now traverse the whole show tree through the existing USB listing and video classifier. Recognized season folders count once, and an unlabelled episode container inside an established show also counts as a season. Videos directly under the show never add a season. Read failures and cancelled scans do not publish a partial count.

Ejection previously discarded the metadata reply receiver, which did not stop its worker or close the file being probed. Series scans, capacity reads and device scans were also not drained before unmounting. These races exist without playback. The browser now suspends new USB reads, cancels recursive series work between directory listings, and waits for existing workers to return before requesting ejection.

The helper also abandoned the first failed clean NTFS unmount and removed the operating-system error detail. It now retains that detail and polls real mount state with bounded clean-unmount retries. The supported PDM request remains the first operation. No forced or lazy unmount is used. A device can remain connected after all its filesystems have unmounted.

These findings identify defects in the implementation. The cause of a particular physical television's persistent failure still requires its returned error and mount evidence.

## Automated regression coverage

- Four unconventional season directories, direct episodes, mixed structures, recursive video discovery and ignored non-video files.
- Cancelled or failed scans produce no partial counts. Existing cache tests cover expiry and stale in-flight results.
- Ejection waits for an in-flight video probe even when playback never started.
- Mocked delayed mount removal, a rejected PDM request, a genuinely busy NTFS partition, clean-unmount success, multiple partitions and delayed non-NTFS unmounts.
- Existing tests cover physical-device mapping, stale tmpfs placeholders, external mounts and unsupported fallback filesystems.

CI runs the USB browser suite and the storage helper tests on Linux, then cross-compiles and verifies the ARM package.

## Physical LG validation still required

Use the repository TV lock and a test USB drive. Record the firmware version, filesystem, returned PDM error, and the selected device's mount rows before and after each attempt. Do not remove a drive while CineUSB reports an uncertain state.

1. Connect NTFS and eject immediately without browsing.
2. Reconnect, wait for background work, then eject.
3. Browse several folders without playback, then eject during metadata scanning.
4. Play a test video, stop, return to the library, then eject.
5. Reconnect the same drive and repeat.
6. Repeat with a physical drive containing multiple partitions. Every partition must be absent from the mount table before success.
7. Hold a test file open in another process. Confirm a failed clean unmount reports the reason, then release it and retry.
8. Confirm success when filesystems are unmounted but PDM still lists the connected device.
9. Exercise delayed PDM replies and mount updates. Confirm the progress animation continues and timeout retains the warning.
10. Repeat with supported FAT and exFAT media. Unsupported fallback mounts must remain failures if PDM cannot unmount them.

Also inspect long series titles and large counts at every card size on the television.
