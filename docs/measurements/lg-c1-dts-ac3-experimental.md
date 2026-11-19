# Experimental DTS to Dolby Digital on the LG C1

CineUSB can convert a local USB track with a 48 kHz DTS 5.1 core to 640 kb/s Dolby Digital 5.1 while it plays at 1×. The FFmpeg encoder takes six discrete channels and creates AC3 access units for the existing AC3 Load. Side and back 5.1 layouts use the same six positions in FFmpeg's channel order. The surround pair is encoded as Dolby Digital left and right surround. No new track is written to the USB drive.

## Test on the C1 and Q990C

1. Install a test build from this branch with its matching bundled FFmpeg libraries. Connect the Q990C to HDMI 2 on the C1, enable eARC and choose Pass Through. Select Standard on the soundbar.
2. In **Ajustes → Reproducción**, select **Convertir DTS (experimental)**. The default is **Desactivado**. The setting is read when a USB video opens.
3. Put the DTS speaker fixture from the [PCM validation procedure](lg-c1-dts-pcm.md) on the USB drive and play it at normal speed. In CineUSB Diagnostics, look for **DTS 6ch → app AC3 6ch 48000 Hz**. This means the app produced an AC3 packet. Check the bar independently for Dolby Digital and verify each speaker with the isolated tones. Record the reported format, firmware and any speaker swaps.
4. Play a longer DTS 5.1 film with a known sync point. Check startup, pause/resume, repeated forward and backward seeks and the end of playback. Compare the offset with the same scenes with the option off. Check regular AC3, EAC3, AAC and FLAC tracks with the option on as controls.
5. If the experimental playback fails, switch the setting off and reopen the video to restore the original route.

The host test decodes the generated AC3 and checks isolation of every speaker and 32 ms access-unit spacing. Only the TV run can verify eARC output and playback performance.
