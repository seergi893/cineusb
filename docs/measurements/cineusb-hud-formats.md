# CineUSB HUD format readout

CineUSB compiles the existing classic HUD without its Info section or opener. The remaining controls and their focus stops come from the same compiler. The title cell uses the actual space left by those controls. The title and format labels share a single line and inherit the HUD's reveal. The title can step down to the headline size and ellipsise. The channel label is the first format label to make room in a narrow cell.

## Sources and confidence

| Readout | Source |
| --- | --- |
| Title | The embedded container title when usable, otherwise the filename stem. Technical suffixes and marked release years are removed conservatively. FFmpeg reads the tag during the normal open. |
| Dolby Vision | The live Starfish video report identifies Dolby Vision and the current route declares it. A Dolby Vision file on a fallback route cannot produce this label from the file flag alone. |
| HDR10 / HDR10+ / HDR / SDR | The live Starfish video object's explicit HDR type, or documented transfer-characteristic values when the type is `none`. If the callback lacks usable information, the label stays absent. |
| Dolby Digital / Dolby Digital Plus / AAC / DTS / PCM / Opus | The current Load audio declaration after the pipeline has accepted audio access units. App conversion therefore follows the format CineUSB sends to Starfish. |
| Channels | The pipeline's decoded PCM count where reported, or the known app-converted format for DTS → PCM and DTS → AC3. Counts alone cannot distinguish 5.1 from other six-channel layouts, so they appear as a count unless the layout is known. |

Container metadata, track selection, the Load and the pipeline report are separate facts. Callback observations are reset with each Feed attempt. Labels follow the active Load on an audio track change. The device's panel mode and the final soundbar signal are separate from the Starfish pipeline. CineUSB has no reliable per-session C1/B1 capability for HDMI or eARC receiver measurements. The public webOS TV Settings API documents system settings queries, and the existing ACB observation is specific to older hardware. CineUSB therefore leaves Atmos and DTS:X unlabelled. The current DTS core path reports DTS and app-decoded TrueHD reports PCM when that is what CineUSB feeds. [LG Settings Service](https://webostv.developer.lge.com/develop/references/settings-service)

## Brand assets

The HUD uses neutral typesetting in the app's regular font. The implementation ships no third-party logos. Dolby supplies a logo-use agreement process, DTS reserves rights in its site terms, and HDR10+ publishes its own logo license agreement. No redistribution permission was established for this build. AAC, PCM, HDR and SDR appear as format names in the same typography. [Dolby licensing](https://professional.dolby.com/licensing/) · [DTS terms](https://dts.com/terms-conditions/) · [HDR10+ logo agreement](https://hdr10plus.org/wp-content/uploads/2025/08/HDR10-Logo-License-Agreement.pdf)

## Validation on LG C1 and B1

Open short and long USB titles in different languages at 1080p and 4K UI settings. Compare an SDR clip, a known HDR10 clip and a Dolby Vision title with its base-layer fallback. Change audio tracks and compare the HUD with Diagnostics. Check that the remaining controls still respond to pointer and D-pad input, including subtitle, seek, pause and resume. Check the television and soundbar's own readouts separately when assessing the final signal.
