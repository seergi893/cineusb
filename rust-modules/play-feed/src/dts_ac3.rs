//! Experimental local DTS core to AC-3 bridge. One owner on the demux thread.
use plx_media_codecs::ffi::*;
use std::ffi::{c_int, c_void, CStr};
use std::ptr::{null_mut, write_unaligned};

const RATE: c_int = 48_000;
const CHANNELS: usize = 6;
const FLTP: c_int = 8;
const S32P: c_int = 7;
const BLOCK: usize = 1536;
const BLOCK_NS: i64 = 32_000_000;
const SIDE: u64 = 0x60f; // FL FR FC LFE SL SR in FFmpeg native order.
const BACK: u64 = 0x3f; // FL FR FC LFE BL BR. The surround pair occupies the same slots.
const GAP_NS: i64 = 20_000_000;
const AC3_ID: c_int = 0x15003;

type AccessUnit = (i64, Vec<u8>);

pub(crate) fn available() -> bool {
    abi_ok() && unsafe { !avcodec_find_encoder_by_name(c"ac3".as_ptr()).is_null() }
}

fn check(result: c_int, operation: &str) -> Result<(), String> {
    if result < 0 {
        Err(format!("{operation} ({result})"))
    } else {
        Ok(())
    }
}

struct Context(*mut AVCodecContext);

impl Context {
    unsafe fn new(codec: *const AVCodec) -> Result<Self, String> {
        if codec.is_null() {
            return Err("codec unavailable".into());
        }
        let ctx = avcodec_alloc_context3(codec);
        if ctx.is_null() {
            Err("codec allocation failed".into())
        } else {
            Ok(Self(ctx))
        }
    }
}

impl Drop for Context {
    fn drop(&mut self) {
        unsafe { avcodec_free_context(&mut self.0) };
    }
}

struct Frame(*mut c_void);

impl Frame {
    unsafe fn new() -> Result<Self, String> {
        let frame = av_frame_alloc();
        if frame.is_null() {
            Err("frame allocation failed".into())
        } else {
            Ok(Self(frame))
        }
    }
}

impl Drop for Frame {
    fn drop(&mut self) {
        unsafe { av_frame_free(&mut self.0) };
    }
}

struct Encoder {
    context: Context,
    frame: Frame,
    packet: *mut AVPacket,
}

impl Encoder {
    unsafe fn open() -> Result<Self, String> {
        let codec = avcodec_find_encoder_by_name(c"ac3".as_ptr());
        let context = Context::new(codec)?;
        let mut par = avcodec_parameters_alloc();
        if par.is_null() {
            return Err("AC3 parameter allocation failed".into());
        }
        (*par).codec_type = AVMEDIA_TYPE_AUDIO;
        (*par).codec_id = AC3_ID;
        (*par).format = FLTP;
        (*par).bit_rate = 640_000;
        (*par).sample_rate = RATE;
        (*par).ch_layout = AVChannelLayout {
            order: 1,
            nb_channels: CHANNELS as c_int,
            u_mask: SIDE,
            opaque: null_mut(),
        };
        let result = avcodec_parameters_to_context(context.0, par);
        avcodec_parameters_free(&mut par);
        check(result, "AC3 parameters")?;
        check(
            av_opt_set(
                context.0.cast(),
                c"time_base".as_ptr(),
                c"1/48000".as_ptr(),
                0,
            ),
            "AC3 time base",
        )?;
        check(
            avcodec_open2(context.0, codec, null_mut()),
            "open AC3 encoder",
        )?;

        let frame = Frame::new()?;
        let base = frame.0.cast::<u8>();
        *base.add(OFF_FRAME_FORMAT).cast::<c_int>() = FLTP;
        *base.add(OFF_AFRAME_NB_SAMPLES).cast::<c_int>() = BLOCK as c_int;
        *base.add(OFF_AFRAME_SAMPLE_RATE).cast::<c_int>() = RATE;
        *base.add(OFF_AFRAME_CH_ORDER).cast::<c_int>() = 1;
        *base.add(OFF_AFRAME_NB_CHANNELS).cast::<c_int>() = CHANNELS as c_int;
        write_unaligned(base.add(OFF_AFRAME_CH_MASK).cast::<u64>(), SIDE);
        check(av_frame_get_buffer(frame.0, 0), "AC3 frame buffer")?;

        let packet = av_packet_alloc();
        if packet.is_null() {
            return Err("AC3 packet allocation failed".into());
        }
        Ok(Self {
            context,
            frame,
            packet,
        })
    }

    unsafe fn encode(&mut self, pcm: &[[f32; BLOCK]; CHANNELS]) -> Result<Vec<u8>, String> {
        check(
            av_frame_make_writable(self.frame.0),
            "AC3 frame not writable",
        )?;
        for (channel, samples) in pcm.iter().enumerate() {
            let plane = *self.frame.0.cast::<*mut f32>().add(channel);
            std::ptr::copy_nonoverlapping(samples.as_ptr(), plane, BLOCK);
        }
        check(
            avcodec_send_frame(self.context.0, self.frame.0),
            "send AC3 frame",
        )?;
        check(
            avcodec_receive_packet(self.context.0, self.packet),
            "receive AC3 packet",
        )?;
        let packet = &*self.packet;
        let bytes = std::slice::from_raw_parts(packet.data, packet.size as usize).to_vec();
        av_packet_unref(self.packet);
        Ok(bytes)
    }
}

impl Drop for Encoder {
    fn drop(&mut self) {
        unsafe { av_packet_free(&mut self.packet) };
    }
}

pub(crate) struct DtsAc3 {
    decoder: Context,
    frame: Frame,
    encoder: Encoder,
    pcm: Box<[[f32; BLOCK]; CHANNELS]>,
    used: usize,
    next_pts: Option<i64>,
}

impl DtsAc3 {
    pub(crate) unsafe fn open(par: *const AVCodecParameters) -> Result<Self, String> {
        if CStr::from_ptr(avcodec_get_name((*par).codec_id)).to_bytes() != b"dts"
            || (*par).sample_rate != RATE
            || (*par).ch_layout.nb_channels != CHANNELS as c_int
        {
            return Err("selected audio is not DTS 5.1 at 48 kHz".into());
        }
        let codec = avcodec_find_decoder((*par).codec_id);
        let decoder = Context::new(codec)?;
        check(
            avcodec_parameters_to_context(decoder.0, par),
            "DTS parameters",
        )?;
        check(
            av_opt_set(decoder.0.cast(), c"core_only".as_ptr(), c"1".as_ptr(), 1),
            "DTS core option",
        )?;
        check(
            avcodec_open2(decoder.0, codec, null_mut()),
            "open DTS core decoder",
        )?;
        Ok(Self {
            decoder,
            frame: Frame::new()?,
            encoder: Encoder::open()?,
            pcm: Box::new([[0.0; BLOCK]; CHANNELS]),
            used: 0,
            next_pts: None,
        })
    }

    pub(crate) unsafe fn reset(&mut self) -> Result<(), String> {
        self.encoder = Encoder::open()?;
        avcodec_flush_buffers(self.decoder.0);
        self.used = 0;
        self.next_pts = None;
        Ok(())
    }

    pub(crate) unsafe fn decode(
        &mut self,
        packet: *const AVPacket,
        pts: i64,
    ) -> Result<Vec<AccessUnit>, String> {
        if self.next_pts.is_some_and(|next| {
            let expected = next.saturating_add(self.used as i64 * 1_000_000_000 / i64::from(RATE));
            expected.abs_diff(pts) > GAP_NS as u64
        }) {
            self.reset()?;
        }
        check(
            avcodec_send_packet(self.decoder.0, packet),
            "send DTS packet",
        )?;
        let mut output = Vec::new();
        loop {
            let result = avcodec_receive_frame(self.decoder.0, self.frame.0);
            if result == AVERROR_EAGAIN || result == AVERROR_EOF {
                return Ok(output);
            }
            check(result, "receive DTS frame")?;
            self.next_pts.get_or_insert(pts);
            self.append_frame(&mut output)?;
        }
    }

    unsafe fn append_frame(&mut self, output: &mut Vec<AccessUnit>) -> Result<(), String> {
        let base = self.frame.0.cast::<u8>();
        let rate = *base.add(OFF_AFRAME_SAMPLE_RATE).cast::<c_int>();
        let channels = *base.add(OFF_AFRAME_NB_CHANNELS).cast::<c_int>();
        let format = *base.add(OFF_FRAME_FORMAT).cast::<c_int>();
        let mask = frame_channel_mask(base);
        if rate != RATE
            || channels != CHANNELS as c_int
            || !matches!(mask, SIDE | BACK)
            || !matches!(format, FLTP | S32P)
        {
            return Err(format!("unsupported DTS core frame: {rate} Hz, {channels} channels, layout {mask:#x}, format {format}"));
        }
        let count = (*base.add(OFF_AFRAME_NB_SAMPLES).cast::<c_int>()).max(0) as usize;
        let mut from = 0;
        while from < count {
            let take = (count - from).min(BLOCK - self.used);
            for (channel, plane) in self.pcm.iter_mut().enumerate() {
                let src = *base.cast::<*const u8>().add(channel);
                let dst = &mut plane[self.used..self.used + take];
                if format == FLTP {
                    dst.copy_from_slice(std::slice::from_raw_parts(
                        src.cast::<f32>().add(from),
                        take,
                    ));
                } else {
                    for (value, &sample) in dst.iter_mut().zip(std::slice::from_raw_parts(
                        src.cast::<i32>().add(from),
                        take,
                    )) {
                        *value = sample as f32 / 2_147_483_648.0;
                    }
                }
            }
            from += take;
            self.used += take;
            if self.used == BLOCK {
                output.push(self.encode_block()?);
            }
        }
        Ok(())
    }

    unsafe fn encode_block(&mut self) -> Result<AccessUnit, String> {
        let pts = self.next_pts.expect("an AC3 frame has a DTS timestamp");
        let bytes = self.encoder.encode(&self.pcm)?;
        self.used = 0;
        self.next_pts = Some(pts.saturating_add(BLOCK_NS));
        Ok((pts, bytes))
    }

    pub(crate) unsafe fn finish(&mut self) -> Result<Option<AccessUnit>, String> {
        if self.used == 0 {
            return Ok(None);
        }
        for plane in self.pcm.iter_mut() {
            plane[self.used..].fill(0.0);
        }
        self.encode_block().map(Some)
    }
}

#[cfg(all(test, feature = "test-support"))]
mod tests {
    use super::*;
    use std::process::Command;

    #[test]
    #[ignore = "requires staged FFmpeg with AC3 encoder plus the ffmpeg command-line tool"]
    fn dts_to_ac3_preserves_all_six_speakers_and_timestamps() {
        let _serial = plx_base::testlock::serial();
        let _binding = plx_media_codecs::host_test_support::bind_host_ffmpeg();
        assert!(available());
        let root = std::env::temp_dir().join(format!("cineusb-dts-ac3-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let raw = root.join("speakers.f32");
        let mkv = root.join("speakers.mkv");
        let ac3 = root.join("speakers.ac3");
        let mut input = Vec::new();
        for speaker in 0..CHANNELS {
            for i in 0..RATE {
                let frequency = if speaker == 3 { 60.0 } else { 440.0 };
                let mut frame = [0.0f32; CHANNELS];
                frame[speaker] =
                    0.3 * (i as f32 * std::f32::consts::TAU * frequency / RATE as f32).sin();
                for sample in frame {
                    input.extend_from_slice(&sample.to_le_bytes());
                }
            }
        }
        std::fs::write(&raw, input).unwrap();
        assert!(Command::new("ffmpeg")
            .args([
                "-nostdin",
                "-v",
                "error",
                "-f",
                "f32le",
                "-ar",
                "48000",
                "-ch_layout",
                "5.1",
                "-i"
            ])
            .arg(&raw)
            .args(["-c:a", "dca", "-strict", "experimental", "-b:a", "768k"])
            .arg(&mkv)
            .status()
            .unwrap()
            .success());

        let units = unsafe {
            let mut fmt = avformat_alloc_context();
            let path = std::ffi::CString::new(mkv.to_str().unwrap()).unwrap();
            assert!(avformat_open_input(&mut fmt, path.as_ptr(), null_mut(), null_mut()) >= 0);
            assert!(avformat_find_stream_info(fmt, null_mut()) >= 0);
            let stream = *(*fmt).streams;
            let mut converter = DtsAc3::open(stream_codecpar(stream)).unwrap();
            let mut packet = av_packet_alloc();
            let mut units = Vec::new();
            while av_read_frame(fmt, packet) >= 0 {
                let pts = av_rescale_q(
                    (*packet).pts,
                    stream_time_base(stream),
                    AVRational {
                        num: 1,
                        den: 1_000_000_000,
                    },
                );
                units.extend(converter.decode(packet, pts).unwrap());
                av_packet_unref(packet);
            }
            units.extend(converter.finish().unwrap());
            assert!(converter.finish().unwrap().is_none());
            converter.reset().unwrap();
            assert!(converter.finish().unwrap().is_none());
            av_packet_free(&mut packet);
            avformat_close_input(&mut fmt);
            units
        };
        assert_eq!(units[0].0, 0);
        assert!(units.windows(2).all(|w| w[1].0 - w[0].0 == BLOCK_NS));
        assert!((187..=189).contains(&units.len()));
        std::fs::write(
            &ac3,
            units
                .into_iter()
                .flat_map(|(_, bytes)| bytes)
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let output = Command::new("ffmpeg")
            .args(["-nostdin", "-v", "error", "-i"])
            .arg(&ac3)
            .args(["-c:a", "pcm_f32le", "-f", "f32le", "pipe:1"])
            .output()
            .unwrap();
        assert!(output.status.success());
        let samples: Vec<f32> = output
            .stdout
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes(b.try_into().unwrap()))
            .collect();
        for speaker in 0..CHANNELS {
            let start = speaker * RATE as usize + RATE as usize / 4;
            let end = start + RATE as usize / 2;
            let energy = |channel| {
                samples[start * CHANNELS + channel..end * CHANNELS]
                    .iter()
                    .step_by(CHANNELS)
                    .map(|v| v * v)
                    .sum::<f32>()
            };
            let signal = energy(speaker);
            assert!(signal > 10.0, "speaker {speaker} is silent");
            for other in (0..CHANNELS).filter(|&i| i != speaker) {
                assert!(
                    energy(other) < signal / 100.0,
                    "speaker {speaker} leaked into {other}"
                );
            }
        }
        std::fs::remove_dir_all(root).unwrap();
    }
}
