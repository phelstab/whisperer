//! Audio files: mixing the recorded tracks into the final WAV, and converting it to the 16 kHz mono
//! WAV whisper.cpp expects.
use crate::record::{CHANNELS, RATE};
use std::fs::File;
use std::io::{BufReader, BufWriter};
use std::path::Path;

type Reader = hound::WavReader<BufReader<File>>;
type Writer = hound::WavWriter<BufWriter<File>>;

/// Sums `tracks` (48 kHz stereo 16-bit, starting together) sample by sample into `out`; a shorter
/// track counts as silence once it ends. Returns the length in seconds.
pub fn mix(tracks: &[&Path], out: &Path) -> Result<f64, String> {
    let err = |e: hound::Error| format!("{}: {e}", out.display());
    let mut readers: Vec<Reader> = tracks.iter()
        .map(|p| hound::WavReader::open(p).map_err(|e| format!("{}: {e}", p.display())))
        .collect::<Result<_, _>>()?;
    let spec = hound::WavSpec { channels: CHANNELS, sample_rate: RATE, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
    let mut writer = hound::WavWriter::create(out, spec).map_err(err)?;
    let mut samples: Vec<_> = readers.iter_mut().map(|r| r.samples::<i16>()).collect();
    let mut count = 0u64;
    loop {
        let mut any = false;
        let mut sum = 0i32;
        for s in samples.iter_mut() {
            if let Some(v) = s.next() {
                any = true;
                sum += v.unwrap_or(0) as i32;
            }
        }
        if !any {
            break;
        }
        writer.write_sample(sum.clamp(i16::MIN as i32, i16::MAX as i32) as i16).map_err(err)?;
        count += 1;
    }
    writer.finalize().map_err(err)?;
    Ok(count as f64 / (RATE as f64 * CHANNELS as f64))
}

/// Converts `input` (48 kHz stereo 16-bit) to 16 kHz mono 16-bit `out`: downmix, low-pass, keep
/// every third sample.
pub fn to_16k_mono(input: &Path, out: &Path) -> Result<(), String> {
    let err = |e: hound::Error| format!("{}: {e}", out.display());
    let mut reader = hound::WavReader::open(input).map_err(|e| format!("{}: {e}", input.display()))?;
    let spec = reader.spec();
    if spec.sample_rate != RATE || spec.channels != CHANNELS || spec.bits_per_sample != 16 {
        return Err(format!("{}: expected 48 kHz stereo 16-bit", input.display()));
    }
    let mut writer: Writer = hound::WavWriter::create(out, hound::WavSpec { channels: 1, sample_rate: 16_000, ..spec }).map_err(err)?;
    let mut decimator = Decimator::new();
    let mut samples = reader.samples::<i16>();
    while let (Some(l), Some(r)) = (samples.next(), samples.next()) {
        let mono = (l.unwrap_or(0) as f32 + r.unwrap_or(0) as f32) * 0.5;
        if let Some(v) = decimator.push(mono) {
            writer.write_sample(v.round().clamp(i16::MIN as f32, i16::MAX as f32) as i16).map_err(err)?;
        }
    }
    writer.finalize().map_err(err)
}

/// Low-pass FIR (windowed sinc, cut-off 7.2 kHz at 48 kHz) followed by keeping every third sample.
struct Decimator {
    taps: Vec<f32>,
    history: Vec<f32>,
    pos: usize,
    phase: u32,
}

impl Decimator {
    const TAPS: usize = 95;

    fn new() -> Self {
        let fc = 7_200.0 / RATE as f64;
        let m = (Self::TAPS - 1) as f64;
        let mut taps: Vec<f64> = (0..Self::TAPS).map(|n| {
            let x = n as f64 - m / 2.0;
            let sinc = if x == 0.0 { 2.0 * fc } else { (2.0 * std::f64::consts::PI * fc * x).sin() / (std::f64::consts::PI * x) };
            let a = 2.0 * std::f64::consts::PI * n as f64 / m;
            sinc * (0.42 - 0.5 * a.cos() + 0.08 * (2.0 * a).cos()) // Blackman window
        }).collect();
        let sum: f64 = taps.iter().sum();
        taps.iter_mut().for_each(|t| *t /= sum);
        Decimator { taps: taps.into_iter().map(|t| t as f32).collect(), history: vec![0.0; Self::TAPS], pos: 0, phase: 0 }
    }

    /// Feeds one 48 kHz sample; returns a 16 kHz sample every third call.
    fn push(&mut self, x: f32) -> Option<f32> {
        self.history[self.pos] = x;
        self.pos = (self.pos + 1) % Self::TAPS;
        self.phase = (self.phase + 1) % 3;
        if self.phase != 0 {
            return None;
        }
        // history[pos] is now the oldest sample
        let (old, new) = self.history.split_at(self.pos);
        Some(new.iter().chain(old).zip(&self.taps).map(|(s, t)| s * t).sum())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Amplitude of a sine of `freq` Hz after decimation (steady state).
    fn gain(freq: f64) -> f32 {
        let mut d = Decimator::new();
        let out: Vec<f32> = (0..48_000)
            .filter_map(|n| d.push((2.0 * std::f64::consts::PI * freq * n as f64 / 48_000.0).sin() as f32))
            .skip(100)
            .collect();
        out.iter().fold(0.0, |m, v| m.max(v.abs()))
    }

    #[test]
    fn keeps_speech_and_removes_aliasing() {
        assert!((gain(1_000.0) - 1.0).abs() < 0.01, "1 kHz passes");
        assert!(gain(4_000.0) > 0.98, "4 kHz passes");
        assert!(gain(12_000.0) < 0.01, "12 kHz (would alias to 4 kHz) is removed");
        assert!(gain(20_000.0) < 0.01, "20 kHz is removed");
    }

    #[test]
    fn mixes_and_converts_files() {
        let dir = std::env::temp_dir().join(format!("whisperer-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let spec = hound::WavSpec { channels: 2, sample_rate: 48_000, bits_per_sample: 16, sample_format: hound::SampleFormat::Int };
        let write = |name: &str, frames: usize, v: i16| {
            let p = dir.join(name);
            let mut w = hound::WavWriter::create(&p, spec).unwrap();
            (0..frames * 2).for_each(|_| w.write_sample(v).unwrap());
            w.finalize().unwrap();
            p
        };
        let a = write("a.wav", 48_000, 30_000);
        let b = write("b.wav", 24_000, 10_000);
        let mixed = dir.join("mixed.wav");
        assert_eq!(mix(&[&a, &b], &mixed).unwrap(), 1.0);
        let s: Vec<i16> = hound::WavReader::open(&mixed).unwrap().samples().map(Result::unwrap).collect();
        assert_eq!((s[0], s[47_999], s[48_000], s[95_999]), (i16::MAX, i16::MAX, 30_000, 30_000), "clips, then the longer track goes on");

        let small = dir.join("16k.wav");
        to_16k_mono(&mixed, &small).unwrap();
        let r = hound::WavReader::open(&small).unwrap();
        assert_eq!((r.spec().sample_rate, r.spec().channels, r.duration()), (16_000, 1, 16_000));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
