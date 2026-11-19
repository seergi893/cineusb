# LG C1 DTS decoding and PCM over eARC

## Available evidence

LG specifies webOS 6.0 and eARC for the C1, with eARC on HDMI 2. LG's webOS guide describes multichannel PCM bypass for an **HDMI input** with Digital Sound Output set to Pass Through. That tests an external source path. CineUSB starts inside the TV and feeds an elementary audio stream to Starfish/libpf. [LG C1 specifications](https://www.lg.com/np/tvs/lg-oled65c1ptb) · [LG C1 HDMI port specifications](https://www.lg.com/us/tvs/lg-oled65c1pub-oled-4k-tv) · [LG webOS audio guide](https://kr.eguide.lgappstv.com/manual/w24_mr4/w24_hk21/engc.html)

CineUSB's [PCM Load builder](../../rust-modules/play-feed/src/lib.rs) writes interleaved PCM with `pcmInfo.channelMode` set to mono or stereo. The [device caps](../../rust-modules/platform/src/devcaps.rs) document mono, stereo and dual as the known libpf modes and cap this path at two channels. They also record a C1/webOS 6.5.3 test showing that PCM sample-rate declarations affect playback. The C1's firmware inventory also lists a ranked PCM decoder, without a channel count or speaker map. Those observations establish a PCM path only. The six-channel capability of this C1 libpf path, its PCM slot order and the resulting eARC signal are all unverified.

NDL DirectMedia is a different API. Its third-party implementation notes include a test in which a six-channel PCM plane played on a G5 while the AVR reported 2.0. This is evidence for checking the downstream signal independently of the app and pipeline reports. [DirectMedia implementation and test notes](https://github.com/punktfunk/client-webos/blob/main/docs/NOTES.md)

The existing DTS fallback remains stereo, 48 kHz, signed 16-bit little-endian. The Diagnostics Audio row shows the selected DTS stream as probed by FFmpeg, the core frame shape when applicable, and the PCM CineUSB produced. A question mark means FFmpeg did not provide the source fact. The PCM bit depth is the emitted sample width. A downmix is indicated when the decoded frame has more channels than the emitted PCM. For the stereo fallback, front and surround channels map to their respective sides, center maps to both, and LFE is omitted.

## Hardware validation

1. Record the C1 model suffix, firmware and webOS versions, and the soundbar model, firmware and speaker configuration. Use a soundbar or analyzer that reports the incoming LPCM channel count. Connect it to HDMI 2, select HDMI ARC sound output, enable eARC Support and select Pass Through. Disable virtual surround and upmixing. Use the repository TV lock if testing on a shared set.
2. Use a known external HDMI source that can output LPCM 5.1. Play a channel-identification clip with isolated front left, front right, center, LFE, surround left and surround right signals. Record the receiver's input format, exact channel count and speaker routing. A generic PCM label or sound from all speakers is inconclusive. If the control fails, resolve the link or measurement setup before judging native playback.
3. At 1× in CineUSB, play a DTS 5.1 fixture through the app-decoded stereo fallback with Diagnostics open. Confirm the route is Feed with app PCM and no server audio conversion. Record the Audio row and soundbar input. Check left-only and right-only tones, centered dialogue, surround tones folded to their respective stereo sides and the absence of the isolated LFE tone from the app's PCM. For a controlled run, use a debug build with the existing `appdecodelossy` trigger armed before boot and leave Force Direct Play off. Disarm it before running the codec controls.
4. For an isolated engineering probe, first inspect the exact C1 firmware's libpf PCM declaration parser and sink channel configuration. Proceed to a six-channel test only if a six-channel declaration and its sample slot order can be established. Feed interleaved 48 kHz S16 with six isolated signals in that documented order through CineUSB's exact Starfish/libpf path. Test a raw PCM fixture before adding DTS decode so the same speaker sequence can isolate either stage.
5. Capture the pipeline's configured format, the soundbar or analyzer's **incoming LPCM channel count** and the six speaker results. Verify side and back layouts individually if both would be supported. Require six discrete channels with every signal on its intended speaker. Any fallback to stereo, upmixing or channel swap leaves that configuration unqualified.
6. Repeat with DTS stereo and DTS 5.1 at 44.1 and 48 kHz, with an A/V sync marker and a longer clip. Check initial playback, pause/resume, seek, track switch, reopen and end of stream. Repeat with eARC disabled or the sink removed, and verify stereo recovery and sync. Play AC3, EAC3, AAC and lossless controls before and after the test build. Record soundbar reports, Diagnostics, Feed errors and measured A/V offset for each run.

The host mapping test `dts_decodes_to_stereo_48k_through_the_bundled_ffmpeg` uses one tone per source speaker and checks the fallback format and duration at both input rates. Run it with `make check-ffmpeg` on the repository's macOS host setup. Host decoding cannot qualify a TV or soundbar.

This fixture yields six one-second tones in FFmpeg's `5.1` (back surround) order, with a
black video track for playback. DTS encoding can label the resulting surround pair as side
channels. Check the encoded layout with `ffprobe` before testing. The same filter can produce
the external HDMI control by encoding its audio as PCM instead of DTS.

```sh
ffmpeg -f lavfi -i 'color=s=1280x720:r=24:d=6' \
  -f lavfi -i "aevalsrc='0.25*sin(2*PI*440*t)*between(t,0,1)|0.25*sin(2*PI*440*t)*between(t,1,2)|0.25*sin(2*PI*440*t)*between(t,2,3)|0.25*sin(2*PI*60*t)*between(t,3,4)|0.25*sin(2*PI*440*t)*between(t,4,5)|0.25*sin(2*PI*440*t)*between(t,5,6)':s=48000:d=6:c=5.1" \
  -c:v libx264 -pix_fmt yuv420p -c:a dca -strict experimental -b:a 768k dts-speakers.mkv
```
