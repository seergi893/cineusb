use super::*;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoRange {
    Sdr,
    Hdr10,
    Hdr10Plus,
    DolbyVision,
    DolbyVisionHdr10,
}

#[derive(Clone, Copy, Default)]
pub struct LocalVideoDetails {
    pub bytes: Option<u64>,
    pub duration_secs: Option<u64>,
    pub range: Option<VideoRange>,
}

/// Inspect a separate read-only descriptor. File size survives an unreadable container header.
pub fn probe_local_details(file: &File) -> LocalVideoDetails {
    let mut details = LocalVideoDetails {
        bytes: file.metadata().ok().map(|meta| meta.len()).filter(|bytes| *bytes > 0),
        ..LocalVideoDetails::default()
    };
    let _ = read_local_format(file, true, |fmt| {
        unsafe {
            let duration = fmt_duration(fmt);
            if duration > 0 {
                details.duration_secs = Some(duration as u64 / 1_000_000);
            }
            let index = av_find_best_stream(fmt, AVMEDIA_TYPE_VIDEO, -1, -1, std::ptr::null_mut(), 0);
            if index >= 0 {
                let params = stream_codecpar(*(*fmt).streams.add(index as usize));
                let dovi = dovi_conf(params).filter(|record| record.dv_profile != 0);
                let hdr = container_hdr(params);
                let pq = hdr.trc == 16 && hdr.pri == 9;
                let plus = dovi.is_none() && (has_dynamic_side_data((*params).coded_side_data, (*params).nb_coded_side_data)
                    || (pq && sample_hdr10plus(fmt, index, params)));
                details.range = video_range(dovi, hdr.trc, hdr.pri, plus);
            }
        }
        Ok(())
    });
    details
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

fn video_range(dovi: Option<AVDOVIDecoderConfigurationRecord>, transfer: c_int, primaries: c_int, plus: bool) -> Option<VideoRange> {
    if let Some(record) = dovi {
        let hdr10_base = record.dv_bl_signal_compatibility_id == 1
            || (record.dv_profile == 7 && record.bl_present_flag != 0
                && (record.dv_bl_signal_compatibility_id == 6 || (transfer == 16 && primaries == 9)));
        return Some(if hdr10_base { VideoRange::DolbyVisionHdr10 } else { VideoRange::DolbyVision });
    }
    if plus { return Some(VideoRange::Hdr10Plus); }
    match (transfer, primaries) {
        (16, 9) => Some(VideoRange::Hdr10),
        // Explicit SDR transfer characteristics from H.273. HLG and unspecified values stay hidden.
        (1 | 4..=7 | 11..=15, _) => Some(VideoRange::Sdr),
        _ => None,
    }
}

unsafe fn has_dynamic_side_data(list: *const AVPacketSideData, count: c_int) -> bool {
    !list.is_null() && count > 0 && std::slice::from_raw_parts(list, count as usize).iter().any(|data| {
        data.type_ == AV_PKT_DATA_DYNAMIC_HDR10_PLUS && !data.data.is_null() && data.size > 0
    })
}

/// Dynamic metadata often lives in video packets only. Sample a small prefix so opening options
/// stays cheap even on large files. Playback will open a different descriptor.
unsafe fn sample_hdr10plus(fmt: *mut AVFormatContext, index: c_int, params: *const AVCodecParameters) -> bool {
    let hevc = (*params).codec_id == AV_CODEC_ID_HEVC;
    if !hevc && (*params).codec_id != AV_CODEC_ID_AV1 { return false; }
    let length_size = if hevc && (*params).extradata_size >= 23 && !(*params).extradata.is_null()
        && *(*params).extradata == 1 {
        Some((usize::from(*(*params).extradata.add(21)) & 3) + 1)
    } else {
        None
    };
    let mut packet = av_packet_alloc();
    if packet.is_null() { return false; }
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(250);
    let mut remaining = 4 * 1024 * 1024;
    let mut found = false;
    for _ in 0..48 {
        if remaining == 0 || std::time::Instant::now() >= deadline || av_read_frame(fmt, packet) < 0 { break; }
        let size = ((*packet).size.max(0) as usize).min(remaining);
        remaining -= size;
        if (*packet).stream_index == index {
            found = has_dynamic_side_data((*packet).side_data, (*packet).side_data_elems);
            if !found && hevc && size > 0 && !(*packet).data.is_null() {
                found = hevc_hdr10plus(std::slice::from_raw_parts((*packet).data, size), length_size);
            }
        }
        av_packet_unref(packet);
        if found { break; }
    }
    av_packet_free(&mut packet);
    found
}

fn hevc_hdr10plus(data: &[u8], length_size: Option<usize>) -> bool {
    let has_hdr10plus = |nal| strip_hdr10plus_sei_with(nal, true) != SeiStrip::Keep;
    if let Some(length_size) = length_size {
        let mut position = 0;
        while let Some(prefix) = data.get(position..position + length_size) {
            let size = prefix.iter().fold(0usize, |size, byte| (size << 8) | usize::from(*byte));
            position += length_size;
            let Some(end) = nal_end(position, size, data.len()) else { break };
            if has_hdr10plus(&data[position..end]) { return true; }
            position = end;
        }
    } else {
        let mut next = annexb_start(data, 0);
        while let Some((start, prefix)) = next {
            next = annexb_start(data, start + prefix);
            let end = next.map_or(data.len(), |(offset, _)| offset);
            if has_hdr10plus(&data[start + prefix..end]) { return true; }
        }
    }
    false
}

#[test]
fn cineusb_video_range_requires_a_signal_and_preserves_the_dolby_base_layer() {
    assert_eq!(video_range(None, 2, 2, false), None);
    assert_eq!(video_range(None, 18, 9, false), None);
    assert_eq!(video_range(None, 1, 1, false), Some(VideoRange::Sdr));
    assert_eq!(video_range(None, 16, 9, false), Some(VideoRange::Hdr10));
    assert_eq!(video_range(None, 16, 9, true), Some(VideoRange::Hdr10Plus));
    let mut record = parse_dovi_conf(&[1, 0, 8, 6, 1, 0, 1, 0, 0]).unwrap();
    record.dv_bl_signal_compatibility_id = 1;
    assert_eq!(video_range(Some(record), 16, 9, false), Some(VideoRange::DolbyVisionHdr10));
    record.dv_profile = 5;
    record.dv_bl_signal_compatibility_id = 0;
    assert_eq!(video_range(Some(record), 2, 2, false), Some(VideoRange::DolbyVision));
}
