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

The HUD and the USB cards use the same bundled PNG textures and fall back to text if artwork is missing for a format or fails to load. Monochrome variants below are transparent raster exports of the retained SVGs. The HDR10+ artwork uses the official site's colors with transparent margins trimmed.

The library describes the source file. The offline metadata probe reads its video dimensions and range, and the selected audio track's codec, profile and channel count. Explicit Atmos and DTS:X profiles can therefore appear on a card even when the HUD reports a different format during playback. A series shows shared formats after its episodes have been inspected, and indicates mixed formats when the inspected files disagree. Unknown metadata leaves the corresponding mark empty.

| Indicator | Artwork |
| --- | --- |
| Dolby Vision | [Wikimedia Commons SVG supplied for this change](https://commons.wikimedia.org/wiki/File:Dolby_Vision_(logo).svg), sourced from an LG PDF. |
| Dolby Digital | [Wikimedia Commons SVG](https://commons.wikimedia.org/wiki/File:Dolby-Digital.svg), which credits Dolby Laboratories and cites Dolby's product materials. |
| Dolby Digital Plus | [Wikimedia Commons SVG](https://commons.wikimedia.org/wiki/File:Dolby-Digital-Plus.svg), which credits Dolby Laboratories and cites Dolby's product materials. |
| DTS | [Wikimedia Commons SVG](https://commons.wikimedia.org/wiki/File:DTS_(2020).svg), sourced from DTS's website. |
| HDR10+ | [HDR10+ Technologies PNG](https://hdr10plus.org/wp-content/uploads/2023/11/hdr_logo-1.png). |

Commons identifies its listed SVGs as public-domain text logos for copyright purposes. The corresponding trademarks belong to their respective owners. The HUD uses the marks to identify session formats. [Dolby brand guidelines](https://games.dolby.com/globalassets/games-2.0/logos-and-guidelines-downloads/vision/dolby-vision-brand-guidelines_v01_200507.pdf) · [DTS terms](https://dts.com/terms-conditions/) · [HDR10+ logo agreement](https://hdr10plus.org/wp-content/uploads/2025/08/HDR10-Logo-License-Agreement.pdf)

## Validation on LG C1 and B1

Open short and long USB titles in different languages at 1080p and 4K UI settings. Compare an SDR clip, a known HDR10 clip and a Dolby Vision title with its base-layer fallback. Change audio tracks and compare the HUD with Diagnostics. Check that the remaining controls still respond to pointer and D-pad input, including subtitle, seek, pause and resume. Check the television and soundbar's own readouts separately when assessing the final signal.
