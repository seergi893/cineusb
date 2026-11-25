use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum VideoRange {
    Sdr,
    Hdr10,
    Hdr10Plus,
    Hdr,
    DolbyVision,
    DolbyVisionHdr10,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, serde::Serialize)]
pub struct LocalAudioFormat {
    pub label: &'static str,
    pub channels: Option<u8>,
}

impl<'de> serde::Deserialize<'de> for LocalAudioFormat {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        struct SavedAudio { label: String, channels: Option<u8> }
        let saved = <SavedAudio as serde::Deserialize>::deserialize(deserializer)?;
        let label = [
            ("ac3", -99), ("eac3", 30), ("eac3", -99), ("truehd", -99),
            ("dts", 61), ("dts", 60), ("dts", 50), ("dts", -99),
            ("aac", -99), ("flac", -99), ("mp3", -99), ("mp2", -99),
            ("opus", -99), ("vorbis", -99), ("alac", -99), ("pcm_s16le", -99),
        ].into_iter().filter_map(|(codec, profile)| audio_label(codec, profile))
            .find(|known| *known == saved.label.as_str())
            .ok_or_else(|| serde::de::Error::custom("unknown audio format"))?;
        Ok(Self { label, channels: saved.channels })
    }
}

#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
pub struct LocalVideoDetails {
    pub bytes: Option<u64>,
    pub resolution: Option<(u32, u32)>,
    pub duration_secs: Option<u64>,
    pub range: Option<VideoRange>,
    pub hdr10_plus: bool,
    pub hdr10_base: bool,
    pub title: Option<String>,
    /// Container formats of the tracks selected by each existing audio preference.
    pub audio: [Option<LocalAudioFormat>; 2],
}

/// Inspect a separate read-only descriptor. File size survives an unreadable container header.
pub fn probe_local_details(file: &File) -> LocalVideoDetails {
    let mut details = LocalVideoDetails {
        bytes: file.metadata().ok().map(|meta| meta.len()).filter(|bytes| *bytes > 0),
        ..LocalVideoDetails::default()
    };
    let retry = read_local_format(file, true, |fmt| {
        let mut retry = None;
        unsafe {
            details.title = local_title_tag(fmt);
            let duration = fmt_duration(fmt);
            if duration > 0 {
                details.duration_secs = Some(duration as u64 / 1_000_000);
            }
            let index = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
            if index >= 0 {
                let params = stream_codecpar(*(*fmt).streams.add(index as usize));
                let (width, height) = ((*params).width, (*params).height);
                if width > 0 && height > 0 { details.resolution = Some((width as u32, height as u32)); }
                read_video_range(fmt, index, params, &mut details);
                retry = retry_video_range((*params).codec_id, (*params).color_trc, details.range).then_some(index);
            }
            let best = av_find_best_stream(fmt, AVMEDIA_TYPE_AUDIO, -1, -1, std::ptr::null_mut(), 0);
            let tracks: Vec<_> = (0..(*fmt).nb_streams).filter_map(|index| {
                let stream = *(*fmt).streams.add(index as usize);
                let params = stream_codecpar(stream);
                ((*params).codec_type == AVMEDIA_TYPE_AUDIO)
                    .then(|| local_audio_track(index as i32, stream, params))
            }).collect();
            details.audio = [false, true].map(|spanish| {
                let track = pick_local_audio(&tracks, best, spanish)
                    .or_else(|| tracks.iter().find(|track| track.index == best))?;
                let params = stream_codecpar(*(*fmt).streams.add(track.index as usize));
                Some(LocalAudioFormat {
                    label: audio_label(&track.codec, (*params).profile)?,
                    channels: u8::try_from(track.channels).ok().filter(|channels| *channels > 0),
                })
            });
        }
        Ok(retry)
    }).ok().flatten();
    if let Some(index) = retry {
        // The compact library probe can run out of bytes before FFmpeg's
        // HEVC parser has recovered the signal from a large access unit.
        // Retry off-frame at the same probe budget used to open playback.
        let _ = read_local_format(file, false, |fmt| {
            unsafe {
                if av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, index, -1, std::ptr::null_mut(), 0) == index {
                    let params = stream_codecpar(*(*fmt).streams.add(index as usize));
                    if (*params).codec_id != AV_CODEC_ID_HEVC { return Ok(()); }
                    let mut observed = LocalVideoDetails::default();
                    read_video_range(fmt, index, params, &mut observed);
                    if observed.range.is_some() {
                        details.range = observed.range;
                        details.hdr10_base = observed.hdr10_base;
                        details.hdr10_plus = observed.hdr10_plus;
                    }
                }
            }
            Ok(())
        });
    }
    details
}

fn retry_video_range(codec: c_int, transfer: c_int, range: Option<VideoRange>) -> bool {
    codec == AV_CODEC_ID_HEVC && transfer != 18 && matches!(range, None | Some(VideoRange::Hdr))
}

#[test]
fn cineusb_retries_inconclusive_hevc_without_reprobing_confirmed_formats() {
    assert!(retry_video_range(AV_CODEC_ID_HEVC, 2, None));
    assert!(retry_video_range(AV_CODEC_ID_HEVC, 16, Some(VideoRange::Hdr)));
    assert!(!retry_video_range(AV_CODEC_ID_HEVC, 18, Some(VideoRange::Hdr)));
    assert!(!retry_video_range(AV_CODEC_ID_HEVC, 16, Some(VideoRange::Hdr10)));
    assert!(!retry_video_range(AV_CODEC_ID_HEVC, 1, Some(VideoRange::Sdr)));
    assert!(!retry_video_range(AV_CODEC_ID_AV1, 2, None));
}

unsafe fn read_video_range(fmt: *mut AVFormatContext, index: c_int,
    params: *const AVCodecParameters, details: &mut LocalVideoDetails) {
    let dovi = dovi_conf(params).filter(|record| record.dv_profile != 0);
    let hdr = container_hdr(params);
    let pq = hdr.trc == 16 && hdr.pri == 9;
    let stream_plus = has_dynamic_side_data((*params).coded_side_data, (*params).nb_coded_side_data);
    let stream_static = meaningful_mastering(hdr.mastering) || meaningful_cll(hdr.cll);
    let (packet_plus, packet_static) = if dovi.is_none() && !stream_plus {
        sample_hdr_metadata(fmt, index, params)
    } else { (false, false) };
    details.hdr10_plus = stream_plus || packet_plus;
    details.range = video_range(dovi, hdr.trc, hdr.pri, details.hdr10_plus, stream_static || packet_static);
    details.hdr10_base = match details.range {
        Some(VideoRange::DolbyVisionHdr10 | VideoRange::Hdr10) => true,
        Some(VideoRange::Hdr10Plus) => pq,
        _ => false,
    };
}

fn audio_label(codec: &str, profile: c_int) -> Option<&'static str> {
    // AV_PROFILE_* values in libavcodec/defs.h. Container tags and filenames
    // are insufficient evidence for the immersive extensions.
    Some(match (codec, profile) {
        ("ac3", _) => "Dolby Digital",
        ("eac3" | "truehd", 30) => "Dolby Atmos",
        ("eac3", _) => "Dolby Digital Plus",
        ("truehd", _) => "Dolby TrueHD",
        ("dts", 61 | 62) => "DTS:X",
        ("dts", 60) => "DTS-HD MA",
        ("dts", 50) => "DTS-HD HRA",
        ("dts", _) => "DTS",
        ("aac" | "aac_latm", _) => "AAC",
        ("flac", _) => "FLAC",
        ("mp3", _) => "MP3",
        ("mp2", _) => "MP2",
        ("opus", _) => "Opus",
        ("vorbis", _) => "Vorbis",
        ("alac", _) => "ALAC",
        (codec, _) if codec.starts_with("pcm_") => "PCM",
        _ => return None,
    })
}

#[test]
fn cineusb_audio_formats_follow_codec_and_profile() {
    for (codec, profile, label) in [
        ("eac3", 30, "Dolby Atmos"), ("truehd", 30, "Dolby Atmos"), ("truehd", -99, "Dolby TrueHD"),
        ("eac3", -99, "Dolby Digital Plus"), ("ac3", -99, "Dolby Digital"), ("dts", 61, "DTS:X"),
        ("dts", 60, "DTS-HD MA"), ("dts", -99, "DTS"), ("aac", 1, "AAC"), ("pcm_s24le", 0, "PCM"),
    ] {
        assert_eq!(audio_label(codec, profile), Some(label));
    }
}

#[test]
fn cineusb_detects_hdr10plus_in_hevc_packets() {
    let sei = [0x4e, 1, 4, 7, 0xb5, 0, 0x3c, 0, 1, 4, 0, 0x80];
    let mut annexb = vec![0, 0, 1];
    annexb.extend(sei);
    assert!(hevc_hdr10plus(&annexb, None));
    let mut length_prefixed = vec![0, sei.len() as u8];
    length_prefixed.extend(sei);
    assert!(hevc_hdr10plus(&length_prefixed, Some(2)));
    length_prefixed[1] = u8::MAX;
    assert!(!hevc_hdr10plus(&length_prefixed, Some(2)));
}

fn video_range(dovi: Option<AVDOVIDecoderConfigurationRecord>, transfer: c_int, primaries: c_int, plus: bool, static_metadata: bool) -> Option<VideoRange> {
    if let Some(record) = dovi {
        let hdr10_base = record.dv_bl_signal_compatibility_id == 1
            || (record.dv_profile == 7 && record.bl_present_flag != 0
                && (record.dv_bl_signal_compatibility_id == 6 || (transfer == 16 && primaries == 9)));
        return Some(if hdr10_base { VideoRange::DolbyVisionHdr10 } else { VideoRange::DolbyVision });
    }
    if plus { return Some(VideoRange::Hdr10Plus); }
    match (transfer, primaries) {
        (16, 9) => Some(VideoRange::Hdr10),
        (16, 2) if static_metadata => Some(VideoRange::Hdr10),
        (16 | 18, _) => Some(VideoRange::Hdr),
        // Explicit SDR transfer characteristics from H.273. Unspecified values stay hidden.
        (1 | 4..=7 | 11..=15, _) => Some(VideoRange::Sdr),
        (0 | 2, _) if static_metadata => Some(VideoRange::Hdr),
        _ => None,
    }
}

unsafe fn has_dynamic_side_data(list: *const AVPacketSideData, count: c_int) -> bool {
    !list.is_null() && count > 0 && std::slice::from_raw_parts(list, count as usize).iter().any(|data| {
        data.type_ == AV_PKT_DATA_DYNAMIC_HDR10_PLUS && !data.data.is_null() && data.size > 0
    })
}

fn meaningful_mastering(mastering: Option<crate::vp9_hdr::Mastering>) -> bool {
    mastering.is_some_and(|data| {
        let (value, scale) = data.max_luminance;
        data.has_luminance && value > 0 && scale > 0
    })
}

fn meaningful_cll(cll: Option<(u32, u32)>) -> bool {
    cll.is_some_and(|(peak, average)| peak > 0 || average > 0)
}

#[test]
fn cineusb_empty_static_hdr_side_data_is_not_evidence() {
    assert!(!meaningful_mastering(crate::vp9_hdr::parse_mastering(&[0; 88])));
    assert!(!meaningful_cll(crate::vp9_hdr::parse_cll(&[0; 8])));
    assert!(meaningful_cll(Some((1000, 400))));
}

/// Dynamic metadata often lives in video packets only. Sample a small prefix so opening options
/// stays cheap even on large files. Playback will open a different descriptor.
unsafe fn sample_hdr_metadata(fmt: *mut AVFormatContext, index: c_int, params: *const AVCodecParameters) -> (bool, bool) {
    let hevc = (*params).codec_id == AV_CODEC_ID_HEVC;
    if !hevc && (*params).codec_id != AV_CODEC_ID_AV1 { return (false, false); }
    let length_size = if hevc && (*params).extradata_size >= 23 && !(*params).extradata.is_null()
        && *(*params).extradata == 1 {
        Some((usize::from(*(*params).extradata.add(21)) & 3) + 1)
    } else {
        None
    };
    let mut packet = av_packet_alloc();
    if packet.is_null() { return (false, false); }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    let mut remaining = 4 * 1024 * 1024;
    let mut found = false;
    let mut static_metadata = false;
    let mut video_packets = 0;
    for _ in 0..128 {
        if remaining == 0 || std::time::Instant::now() >= deadline || av_read_frame(fmt, packet) < 0 { break; }
        let size = ((*packet).size.max(0) as usize).min(remaining);
        if (*packet).stream_index == index {
            remaining -= size;
            video_packets += 1;
            found = has_dynamic_side_data((*packet).side_data, (*packet).side_data_elems);
            let (list, count) = ((*packet).side_data, (*packet).side_data_elems);
            if !static_metadata && !list.is_null() && count > 0 {
                static_metadata = std::slice::from_raw_parts(list, count as usize).iter().any(|data| {
                    if data.data.is_null() { return false; }
                    let bytes = std::slice::from_raw_parts(data.data, data.size);
                    match data.type_ {
                        AV_PKT_DATA_MASTERING_DISPLAY_METADATA => meaningful_mastering(crate::vp9_hdr::parse_mastering(bytes)),
                        AV_PKT_DATA_CONTENT_LIGHT_LEVEL => meaningful_cll(crate::vp9_hdr::parse_cll(bytes)),
                        _ => false,
                    }
                });
            }
            if !found && hevc && size > 0 && !(*packet).data.is_null() {
                let (dynamic, mastering) = hevc_packet_hdr(std::slice::from_raw_parts((*packet).data, size), length_size);
                found = dynamic;
                static_metadata |= mastering;
            }
        }
        av_packet_unref(packet);
        if found || video_packets >= 24 { break; }
    }
    av_packet_free(&mut packet);
    (found, static_metadata)
}

#[cfg(test)]
fn hevc_hdr10plus(data: &[u8], length_size: Option<usize>) -> bool {
    hevc_packet_hdr(data, length_size).0
}

fn hevc_packet_hdr(data: &[u8], length_size: Option<usize>) -> (bool, bool) {
    let mut static_metadata = false;
    let mut has_hdr10plus = |nal: &[u8]| {
        static_metadata |= hevc_static_sei(nal);
        strip_hdr10plus_sei_with(nal, true) != SeiStrip::Keep
    };
    if let Some(length_size) = length_size {
        let mut position = 0;
        while let Some(prefix) = data.get(position..position + length_size) {
            let size = prefix.iter().fold(0usize, |size, byte| (size << 8) | usize::from(*byte));
            position += length_size;
            let Some(end) = nal_end(position, size, data.len()) else { break };
            if has_hdr10plus(&data[position..end]) { return (true, static_metadata); }
            position = end;
        }
    } else {
        let mut next = annexb_start(data, 0);
        while let Some((start, prefix)) = next {
            next = annexb_start(data, start + prefix);
            let end = next.map_or(data.len(), |(offset, _)| offset);
            if has_hdr10plus(&data[start + prefix..end]) { return (true, static_metadata); }
        }
    }
    (false, static_metadata)
}

fn hevc_static_sei(nal: &[u8]) -> bool {
    if nal.len() < 2 || !matches!((nal[0] >> 1) & 0x3f, 39 | 40) {
        return false;
    }
    fn read_value(bytes: &mut &[u8]) -> Option<usize> {
        let mut value: usize = 0;
        while let Some((&byte, rest)) = bytes.split_first() {
            *bytes = rest;
            value = value.checked_add(usize::from(byte))?;
            if byte != 255 { return Some(value); }
        }
        None
    }
    let rbsp = super::sei_rbsp(&nal[2..]);
    let mut bytes = rbsp.as_slice();
    while bytes.first().is_some_and(|&byte| byte != 0x80) {
        let (Some(kind), Some(size)) = (read_value(&mut bytes), read_value(&mut bytes)) else { break };
        let Some((payload, rest)) = bytes.split_at_checked(size) else { break };
        match (kind, payload) {
            (137, payload) if payload.len() == 24 && payload[16..20].iter().any(|&byte| byte != 0) => return true,
            (144, [a, b, c, d]) if *a | *b | *c | *d != 0 => return true,
            _ => {}
        }
        bytes = rest;
    }
    false
}

#[test]
fn cineusb_hevc_reads_static_sei_without_container_side_data() {
    let annexb = [0, 0, 1, 78, 1, 144, 4, 0, 97, 0, 55, 0x80];
    assert_eq!(hevc_packet_hdr(&annexb, None), (false, true));
    let empty = [0, 0, 0, 9, 78, 1, 144, 4, 0, 0, 0, 0, 0x80];
    assert_eq!(hevc_packet_hdr(&empty, Some(4)), (false, false));
    assert_eq!(hevc_packet_hdr(&annexb[..9], None), (false, false));
}

#[test]
fn cineusb_video_range_requires_a_signal_and_preserves_the_dolby_base_layer() {
    assert_eq!(video_range(None, 2, 2, false, false), None);
    assert_eq!(video_range(None, 2, 2, false, true), Some(VideoRange::Hdr));
    assert_eq!(video_range(None, 18, 9, false, false), Some(VideoRange::Hdr));
    assert_eq!(video_range(None, 1, 1, false, true), Some(VideoRange::Sdr));
    assert_eq!(video_range(None, 16, 9, false, false), Some(VideoRange::Hdr10));
    assert_eq!(video_range(None, 16, 2, false, true), Some(VideoRange::Hdr10));
    assert_eq!(video_range(None, 2, 2, true, false), Some(VideoRange::Hdr10Plus));
    let mut record = parse_dovi_conf(&[1, 0, 8, 6, 1, 0, 1, 0, 0]).unwrap();
    record.dv_bl_signal_compatibility_id = 1;
    assert_eq!(video_range(Some(record), 16, 9, false, false), Some(VideoRange::DolbyVisionHdr10));
    record.dv_profile = 5;
    record.dv_bl_signal_compatibility_id = 0;
    assert_eq!(video_range(Some(record), 2, 2, false, false), Some(VideoRange::DolbyVision));
}
