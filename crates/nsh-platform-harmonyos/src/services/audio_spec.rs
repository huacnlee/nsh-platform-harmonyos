//! Pure WAV/PCM decoder and polyphonic software mixer shared with host tests.

use std::{sync::Arc, time::Duration};

use rustc_hash::FxHashMap;

/// Output sample rate of the native OHAudio PCM short-sound mixer.
pub(crate) const MIXER_SAMPLE_RATE: u32 = 48_000;
/// Maximum simultaneously active sound voices (oldest evicted when full).
pub(crate) const MAX_VOICES: usize = 32;

/// Decoded interleaved stereo `f32` PCM buffer at [`MIXER_SAMPLE_RATE`].
#[derive(Clone, Debug)]
pub(crate) struct DecodedPcm {
    pub(crate) samples: Arc<[f32]>,
    pub(crate) duration: Duration,
    pub(crate) bytes: u64,
    pub(crate) truncated: bool,
}

const WAVE_FORMAT_PCM: u16 = 1;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 3;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// Parses a RIFF/WAVE file into interleaved stereo `f32` at [`MIXER_SAMPLE_RATE`],
/// truncating cleanly to `max_duration` when positive.
///
/// Returns `None` for non-WAV containers or unsupported encodings (such as ADPCM),
/// letting the caller fall back to `AvPlayer`.
pub(crate) fn parse_wav(bytes: &[u8], max_duration: Duration) -> Option<DecodedPcm> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut format_tag = 0u16;
    let mut channels = 0u16;
    let mut sample_rate = 0u32;
    let mut bits_per_sample = 0u16;
    let mut has_fmt = false;
    let mut data_slice: Option<&[u8]> = None;

    let mut cursor = 12usize;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(bytes[cursor + 4..cursor + 8].try_into().ok()?) as usize;
        let payload_start = cursor + 8;
        let payload_end = payload_start.checked_add(size)?.min(bytes.len());
        let payload = &bytes[payload_start..payload_end];

        if id == b"fmt " && payload.len() >= 16 {
            let raw_tag = u16::from_le_bytes([payload[0], payload[1]]);
            channels = u16::from_le_bytes([payload[2], payload[3]]);
            sample_rate = u32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
            bits_per_sample = u16::from_le_bytes([payload[14], payload[15]]);
            format_tag = if raw_tag == WAVE_FORMAT_EXTENSIBLE && payload.len() >= 40 {
                u16::from_le_bytes([payload[24], payload[25]])
            } else {
                raw_tag
            };
            has_fmt = true;
        } else if id == b"data" {
            data_slice = Some(payload);
            if has_fmt {
                break;
            }
        }

        let padded = size.saturating_add(size & 1);
        cursor = payload_start.checked_add(padded)?;
    }

    if !has_fmt {
        return None;
    }
    let data = data_slice?;
    if !(1..=8).contains(&channels) || !(8_000..=192_000).contains(&sample_rate) {
        return None;
    }
    let bytes_per_sample = match (format_tag, bits_per_sample) {
        (WAVE_FORMAT_PCM, 8) => 1usize,
        (WAVE_FORMAT_PCM, 16) => 2,
        (WAVE_FORMAT_PCM, 24) => 3,
        (WAVE_FORMAT_PCM, 32) => 4,
        (WAVE_FORMAT_IEEE_FLOAT, 32) => 4,
        _ => return None,
    };
    let frame_stride = bytes_per_sample * usize::from(channels);
    let total_frames = data.len() / frame_stride;
    if total_frames == 0 {
        return None;
    }

    let total_secs = total_frames as f64 / f64::from(sample_rate);
    let (kept_frames, truncated) =
        if !max_duration.is_zero() && total_secs > max_duration.as_secs_f64() {
            let max_frames = (max_duration.as_secs_f64() * f64::from(sample_rate)).floor() as usize;
            (max_frames.clamp(1, total_frames), true)
        } else {
            (total_frames, false)
        };
    let duration = Duration::from_secs_f64(kept_frames as f64 / f64::from(sample_rate));

    // Decode kept frames into interleaved stereo at source sample rate.
    let mut src_stereo = Vec::with_capacity(kept_frames * 2);
    let ch = usize::from(channels);
    for frame_idx in 0..kept_frames {
        let base = frame_idx * frame_stride;
        let left = decode_sample(
            &data[base..base + bytes_per_sample],
            format_tag,
            bits_per_sample,
        );
        let right = if ch == 1 {
            left
        } else {
            let r_off = base + bytes_per_sample;
            decode_sample(
                &data[r_off..r_off + bytes_per_sample],
                format_tag,
                bits_per_sample,
            )
        };
        src_stereo.push(left);
        src_stereo.push(right);
    }

    let stereo = if sample_rate == MIXER_SAMPLE_RATE {
        src_stereo
    } else {
        resample_stereo(&src_stereo, sample_rate, MIXER_SAMPLE_RATE)
    };
    let bytes = (stereo.len() * std::mem::size_of::<f32>()) as u64;
    Some(DecodedPcm {
        samples: Arc::from(stereo),
        duration,
        bytes,
        truncated,
    })
}

#[inline]
fn decode_sample(bytes: &[u8], format_tag: u16, bits_per_sample: u16) -> f32 {
    match (format_tag, bits_per_sample) {
        (WAVE_FORMAT_PCM, 8) => (f32::from(bytes[0]) - 128.0) / 128.0,
        (WAVE_FORMAT_PCM, 16) => f32::from(i16::from_le_bytes([bytes[0], bytes[1]])) / 32768.0,
        (WAVE_FORMAT_PCM, 24) => {
            let sign = if bytes[2] & 0x80 != 0 { 0xFF } else { 0x00 };
            let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], sign]);
            raw as f32 / 8_388_608.0
        }
        (WAVE_FORMAT_PCM, 32) => {
            let raw = i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            (raw as f64 / 2_147_483_648.0) as f32
        }
        (WAVE_FORMAT_IEEE_FLOAT, 32) => {
            let v = f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
            if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            }
        }
        _ => 0.0,
    }
}

fn resample_stereo(src: &[f32], src_rate: u32, dst_rate: u32) -> Vec<f32> {
    let src_frames = src.len() / 2;
    if src_frames == 0 {
        return Vec::new();
    }
    let dst_frames = ((src_frames as f64 * f64::from(dst_rate)) / f64::from(src_rate))
        .round()
        .max(1.0) as usize;
    let ratio = f64::from(src_rate) / f64::from(dst_rate);
    let mut out = Vec::with_capacity(dst_frames * 2);
    let last = src_frames - 1;
    for i in 0..dst_frames {
        let pos = i as f64 * ratio;
        let idx = (pos.floor() as usize).min(last);
        let next = (idx + 1).min(last);
        let frac = (pos - idx as f64).clamp(0.0, 1.0) as f32;
        let l0 = src[idx * 2];
        let r0 = src[idx * 2 + 1];
        let l1 = src[next * 2];
        let r1 = src[next * 2 + 1];
        out.push(l0 + (l1 - l0) * frac);
        out.push(r0 + (r1 - r0) * frac);
    }
    out
}

/// Estimates decoded stereo 16-bit PCM budget in bytes for compressed sounds.
pub(crate) fn estimate_pcm_bytes(duration: Duration, sample_rate: u32) -> u64 {
    let frames = (duration.as_secs_f64() * f64::from(sample_rate.max(8_000))).floor();
    (frames.max(0.0) as u64).saturating_mul(4)
}

/// Clamps playback rate to the continuous range supported by HarmonyOS `AvPlayer`.
pub(crate) fn clamp_av_rate(rate: f32) -> f32 {
    if rate.is_finite() {
        rate.clamp(0.5, 2.0)
    } else {
        1.0
    }
}

struct Voice {
    sound_id: u32,
    samples: Arc<[f32]>,
    frame_pos: f64,
    volume: f32,
    rate: f64,
}

/// Polyphonic software mixer for short decoded PCM sounds.
#[derive(Default)]
pub(crate) struct MixerState {
    sounds: FxHashMap<u32, DecodedPcm>,
    voices: Vec<Voice>,
}

impl MixerState {
    pub(crate) fn insert_sound(&mut self, id: u32, pcm: DecodedPcm) {
        self.stop_sound(id);
        self.sounds.insert(id, pcm);
    }

    #[cfg(test)]
    pub(crate) fn contains_sound(&self, id: u32) -> bool {
        self.sounds.contains_key(&id)
    }

    pub(crate) fn play(&mut self, id: u32, volume: f32, rate: f32) -> bool {
        let Some(sound) = self.sounds.get(&id) else {
            return false;
        };
        if sound.samples.len() < 2 {
            return false;
        }
        let vol = if volume.is_finite() {
            volume.clamp(0.0, 1.0)
        } else {
            1.0
        };
        let speed = if rate.is_finite() {
            f64::from(rate.clamp(0.25, 4.0))
        } else {
            1.0
        };
        if self.voices.len() >= MAX_VOICES {
            self.voices.remove(0);
        }
        self.voices.push(Voice {
            sound_id: id,
            samples: sound.samples.clone(),
            frame_pos: 0.0,
            volume: vol,
            rate: speed,
        });
        true
    }

    pub(crate) fn stop_sound(&mut self, id: u32) {
        self.voices.retain(|voice| voice.sound_id != id);
    }

    pub(crate) fn unload_sound(&mut self, id: u32) {
        self.stop_sound(id);
        self.sounds.remove(&id);
    }

    pub(crate) fn clear(&mut self) {
        self.voices.clear();
        self.sounds.clear();
    }

    pub(crate) fn has_active_voices(&self) -> bool {
        !self.voices.is_empty()
    }

    /// Mixes active voices into `out` as interleaved stereo S16LE frames.
    /// Returns `true` if any voice remains active after filling `out`.
    pub(crate) fn mix_into_s16le(&mut self, out: &mut [u8]) -> bool {
        let frames = out.len() / 4;
        if self.voices.is_empty() || frames == 0 {
            out.fill(0);
            return false;
        }
        for frame in 0..frames {
            let mut left = 0.0f32;
            let mut right = 0.0f32;
            for voice in &mut self.voices {
                let total_frames = voice.samples.len() / 2;
                let idx = voice.frame_pos.floor() as usize;
                if idx < total_frames {
                    let next = (idx + 1).min(total_frames - 1);
                    let frac = (voice.frame_pos - idx as f64) as f32;
                    let l0 = voice.samples[idx * 2];
                    let r0 = voice.samples[idx * 2 + 1];
                    let l1 = voice.samples[next * 2];
                    let r1 = voice.samples[next * 2 + 1];
                    left += (l0 + (l1 - l0) * frac) * voice.volume;
                    right += (r0 + (r1 - r0) * frac) * voice.volume;
                    voice.frame_pos += voice.rate;
                }
            }
            let l_i16 = (left.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            let r_i16 = (right.clamp(-1.0, 1.0) * 32767.0).round() as i16;
            let off = frame * 4;
            out[off..off + 2].copy_from_slice(&l_i16.to_le_bytes());
            out[off + 2..off + 4].copy_from_slice(&r_i16.to_le_bytes());
        }
        // Zero any trailing sub-frame bytes if buffer wasn't 4-byte aligned.
        let written = frames * 4;
        if written < out.len() {
            out[written..].fill(0);
        }
        self.voices
            .retain(|voice| (voice.frame_pos.floor() as usize) < voice.samples.len() / 2);
        !self.voices.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_wav_pcm16(sample_rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_bytes = (samples.len() * 2) as u32;
        let byte_rate = sample_rate * u32::from(channels) * 2;
        let block_align = channels * 2;
        let mut out = Vec::new();
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&(36 + data_bytes).to_le_bytes());
        out.extend_from_slice(b"WAVE");
        out.extend_from_slice(b"fmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&sample_rate.to_le_bytes());
        out.extend_from_slice(&byte_rate.to_le_bytes());
        out.extend_from_slice(&block_align.to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&data_bytes.to_le_bytes());
        for &s in samples {
            out.extend_from_slice(&s.to_le_bytes());
        }
        out
    }

    #[test]
    fn parse_wav_decodes_mono_resamples_and_truncates() {
        // 1 second of 24 kHz mono sine/constant at 0.5 amplitude.
        let samples = vec![16384i16; 24_000];
        let wav = make_wav_pcm16(24_000, 1, &samples);

        let full = parse_wav(&wav, Duration::from_secs(5)).unwrap();
        assert!(!full.truncated);
        assert_eq!(full.duration, Duration::from_secs(1));
        assert_eq!(full.samples.len(), (MIXER_SAMPLE_RATE as usize) * 2);
        assert!((full.samples[0] - 0.5).abs() < 0.001);
        assert!((full.samples[1] - 0.5).abs() < 0.001);

        // Truncate to 250ms: kept duration = 250ms, truncated = true, still playable!
        let short = parse_wav(&wav, Duration::from_millis(250)).unwrap();
        assert!(short.truncated);
        assert_eq!(short.duration, Duration::from_millis(250));
        assert_eq!(short.samples.len(), (MIXER_SAMPLE_RATE as usize / 4) * 2);

        // Non-WAV bytes return None so caller falls back to AvPlayer.
        assert!(parse_wav(b"ID3\x04\x00not a wav", Duration::from_secs(5)).is_none());
    }

    #[test]
    fn mixer_mixes_polyphony_evicts_oldest_and_stops_cleanly() {
        let wav = make_wav_pcm16(MIXER_SAMPLE_RATE, 2, &[16384, -16384, 16384, -16384]);
        let pcm = parse_wav(&wav, Duration::from_secs(5)).unwrap();
        let mut mixer = MixerState::default();
        mixer.insert_sound(1, pcm);

        assert!(mixer.play(1, 0.5, 1.0));
        assert!(mixer.play(1, 0.5, 1.0));
        let mut buf = [0u8; 8]; // 2 stereo S16LE frames
        let active = mixer.mix_into_s16le(&mut buf);
        assert!(!active);
        let l = i16::from_le_bytes([buf[0], buf[1]]);
        let r = i16::from_le_bytes([buf[2], buf[3]]);
        assert!((l - 16384).abs() <= 2);
        assert!((r + 16384).abs() <= 2);

        // Evicts oldest when exceeding MAX_VOICES.
        for _ in 0..(MAX_VOICES + 5) {
            assert!(mixer.play(1, 0.1, 1.0));
        }
        assert_eq!(mixer.voices.len(), MAX_VOICES);

        mixer.stop_sound(1);
        assert!(!mixer.has_active_voices());
        assert!(mixer.contains_sound(1));
        mixer.unload_sound(1);
        assert!(!mixer.contains_sound(1));
    }
}
