// What a recording is, how big it may get, and whether it holds anything worth
// sending. Audio is 16 kHz, mono, 16-bit PCM, and travels as a WAV file held in
// memory:
//   * every speech service accepts WAV, so nothing has to be compressed (no codec,
//     no extra crate);
//   * Windows' waveIn converts whatever the microphone does into exactly this, so
//     there is no resampling code either;
//   * it is 32 KB per second, which keeps even the longest push small.

/// Samples per second. 16 kHz is what speech models work at.
pub const SAMPLE_RATE: u32 = 16_000;

/// The longest push-to-talk. Spoken commands are a few seconds; this is generous
/// enough for a paragraph and still only ~1.4 MB. Reaching it stops the recording
/// (see `Voice`), and the buffer itself refuses to grow past it (`MAX_SAMPLES`),
/// so a stuck key or a capture bug cannot make an unbounded recording.
pub const MAX_RECORDING_SECS: u32 = 45;

pub const MAX_SAMPLES: usize = (SAMPLE_RATE * MAX_RECORDING_SECS) as usize;

/// Largest WAV that is ever built: the header plus a full recording.
pub const MAX_WAV_BYTES: usize = WAV_HEADER_BYTES + MAX_SAMPLES * 2;

const WAV_HEADER_BYTES: usize = 44;

/// Shorter than this is a tap, not a request.
pub const MIN_SPEECH_MS: u32 = 300;

/// Quieter than this (out of 32 768) the recording is room noise, which speech
/// models turn into invented words. Ordinary speech peaks far above it.
pub const SILENCE_PEAK: u16 = 200;

/// A finished recording: PCM samples, and whether it was cut at the limit.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Recording {
    pub samples: Vec<i16>,
    pub hit_limit: bool,
}

/// What a recording holds, judged locally before anything is uploaded.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Heard {
    /// Worth transcribing.
    Speech,
    /// Pressed and released again straight away.
    TooShort,
    /// Quiet: nobody said anything.
    Silence,
    /// Every sample is exactly zero. A real microphone is never that quiet: the
    /// device is muted, or Windows is not letting desktop apps use it.
    Blocked,
}

pub fn judge(recording: &Recording) -> Heard {
    let samples = &recording.samples;
    if samples.len() < (SAMPLE_RATE * MIN_SPEECH_MS / 1000) as usize {
        return Heard::TooShort;
    }
    let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap_or(0);
    match peak {
        0 => Heard::Blocked,
        p if p < SILENCE_PEAK => Heard::Silence,
        _ => Heard::Speech,
    }
}

/// A WAV file (RIFF, PCM, mono, 16-bit, `SAMPLE_RATE`). At most `MAX_SAMPLES`
/// samples are used, so the result is never bigger than `MAX_WAV_BYTES`.
pub fn encode_wav(samples: &[i16]) -> Vec<u8> {
    let samples = &samples[..samples.len().min(MAX_SAMPLES)];
    let data_len = (samples.len() * 2) as u32;
    let mut wav = Vec::with_capacity(WAV_HEADER_BYTES + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // size of the fmt chunk
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(SAMPLE_RATE * 2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        wav.extend_from_slice(&sample.to_le_bytes());
    }
    debug_assert!(wav.len() <= MAX_WAV_BYTES);
    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec(samples: Vec<i16>) -> Recording {
        Recording {
            samples,
            hit_limit: false,
        }
    }

    /// `ms` of a constant-amplitude signal.
    fn tone(ms: u32, amplitude: i16) -> Vec<i16> {
        (0..SAMPLE_RATE * ms / 1000)
            .map(|i| if i % 2 == 0 { amplitude } else { -amplitude })
            .collect()
    }

    #[test]
    fn the_limits_are_what_the_comments_say() {
        assert_eq!(MAX_SAMPLES, 720_000);
        assert_eq!(MAX_WAV_BYTES, 1_440_044);
    }

    // Far below what any speech service accepts (25 MB and up).
    const _: () = assert!(MAX_WAV_BYTES < 2_000_000);

    #[test]
    fn a_recording_is_judged_before_it_is_uploaded() {
        assert_eq!(judge(&rec(vec![])), Heard::TooShort);
        assert_eq!(judge(&rec(tone(200, 9000))), Heard::TooShort);
        assert_eq!(judge(&rec(vec![0; 16_000])), Heard::Blocked);
        assert_eq!(judge(&rec(tone(1000, 40))), Heard::Silence);
        assert_eq!(
            judge(&rec(tone(1000, SILENCE_PEAK as i16 - 1))),
            Heard::Silence
        );
        assert_eq!(judge(&rec(tone(1000, SILENCE_PEAK as i16))), Heard::Speech);
        assert_eq!(judge(&rec(tone(1000, 9000))), Heard::Speech);
        // Full-scale negative has no positive twin; it must not overflow.
        assert_eq!(judge(&rec(vec![i16::MIN; 16_000])), Heard::Speech);
    }

    #[test]
    fn one_loud_moment_among_silence_is_speech() {
        let mut samples = vec![3i16; 16_000];
        samples[8_000] = 12_000;
        assert_eq!(judge(&rec(samples)), Heard::Speech);
    }

    #[test]
    fn the_wav_has_a_valid_header_and_the_samples_in_order() {
        let wav = encode_wav(&[1, -2, 300]);
        assert_eq!(wav.len(), 44 + 6);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36 + 6);
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        assert_eq!(
            u16::from_le_bytes(wav[20..22].try_into().unwrap()),
            1,
            "PCM"
        );
        assert_eq!(
            u16::from_le_bytes(wav[22..24].try_into().unwrap()),
            1,
            "mono"
        );
        assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(wav[28..32].try_into().unwrap()), 32_000);
        assert_eq!(u16::from_le_bytes(wav[32..34].try_into().unwrap()), 2);
        assert_eq!(u16::from_le_bytes(wav[34..36].try_into().unwrap()), 16);
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(u32::from_le_bytes(wav[40..44].try_into().unwrap()), 6);
        assert_eq!(i16::from_le_bytes(wav[44..46].try_into().unwrap()), 1);
        assert_eq!(i16::from_le_bytes(wav[46..48].try_into().unwrap()), -2);
        assert_eq!(i16::from_le_bytes(wav[48..50].try_into().unwrap()), 300);
    }

    #[test]
    fn an_oversized_recording_is_cut_at_the_limit_not_sent_whole() {
        let wav = encode_wav(&vec![5i16; MAX_SAMPLES + 10_000]);
        assert_eq!(wav.len(), MAX_WAV_BYTES);
        assert_eq!(
            u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize,
            MAX_SAMPLES * 2
        );
    }

    #[test]
    fn an_empty_wav_is_still_well_formed() {
        let wav = encode_wav(&[]);
        assert_eq!(wav.len(), 44);
        assert_eq!(u32::from_le_bytes(wav[4..8].try_into().unwrap()), 36);
    }
}
