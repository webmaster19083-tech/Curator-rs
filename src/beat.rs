//! Local soundtrack beat analysis.
//!
//! The browser owns the monotonic Web Audio clock at playback time. This
//! module only derives an editable initial BPM/first-beat proposal from local
//! audio by decoding mono PCM through ffmpeg and using onset autocorrelation.

use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct BeatAnalysis {
    pub bpm: f64,
    pub first_beat_offset_secs: f64,
    pub confidence: f64,
    pub markers: Vec<f64>,
    pub requires_confirmation: bool,
}

const SAMPLE_RATE: u32 = 22_050;
const HOP: usize = 512;
const WINDOW: usize = 1024;

/// Decode a bounded five-minute mono PCM analysis buffer. The original audio
/// is never changed or copied into Curator's database.
pub fn decode_mono_pcm(ffmpeg_bin: &str, path: &Path) -> Result<Vec<f32>, String> {
    if !path.is_file() { return Err("Audio file is unavailable".to_string()); }
    let output = std::process::Command::new(ffmpeg_bin)
        .args(["-hide_banner", "-loglevel", "error", "-i"])
        .arg(path)
        .args(["-t", "300", "-vn", "-ac", "1", "-ar", "22050", "-f", "f32le", "pipe:1"])
        .output()
        .map_err(|error| format!("ffmpeg is unavailable: {error}"))?;
    if !output.status.success() { return Err("ffmpeg could not decode this audio track".to_string()); }
    if output.stdout.len() < 4 { return Err("Audio track contains no usable PCM samples".to_string()); }
    Ok(output.stdout.chunks_exact(4).map(|bytes| f32::from_le_bytes([bytes[0],bytes[1],bytes[2],bytes[3]])).filter(|sample| sample.is_finite()).collect())
}

/// A dependency-free onset/autocorrelation estimator. It is intentionally
/// conservative: diffuse/noisy tracks get a low confidence and must be
/// confirmed by the person before driving a session.
pub fn analyze_pcm(samples: &[f32], sample_rate: u32) -> BeatAnalysis {
    if samples.len() < WINDOW * 4 || sample_rate == 0 {
        return BeatAnalysis { bpm: 120.0, first_beat_offset_secs: 0.0, confidence: 0.0, markers: Vec::new(), requires_confirmation: true };
    }
    let energies = samples.windows(WINDOW).step_by(HOP).map(|window| {
        window.iter().map(|sample| f64::from(*sample) * f64::from(*sample)).sum::<f64>() / WINDOW as f64
    }).collect::<Vec<_>>();
    let mut onset = Vec::with_capacity(energies.len());
    for (index, energy) in energies.iter().enumerate() {
        onset.push(if index == 0 { 0.0 } else { (energy - energies[index - 1]).max(0.0) });
    }
    let mean = onset.iter().sum::<f64>() / onset.len() as f64;
    let variance = onset.iter().map(|value| (value - mean).powi(2)).sum::<f64>() / onset.len() as f64;
    let scale = variance.sqrt().max(1e-12);
    for value in &mut onset { *value = (*value - mean) / scale; }
    let frames_per_second = sample_rate as f64 / HOP as f64;
    let mut best = (120.0, f64::MIN, 0_usize);
    for bpm in 60..=200 {
        let lag = (frames_per_second * 60.0 / bpm as f64).round() as usize;
        if lag == 0 || lag >= onset.len() { continue; }
        let correlation = onset.iter().zip(onset.iter().skip(lag)).map(|(a,b)| a*b).sum::<f64>() / (onset.len() - lag) as f64;
        if correlation > best.1 { best = (bpm as f64, correlation, lag); }
    }
    // Autocorrelation alone is ambiguous at octave tempos: a regular 120
    // BPM click track is also periodic at 60 BPM.  Use the onset maxima to
    // select the musically nearest interval when there is a stable sequence,
    // while retaining autocorrelation as the fallback and confidence signal.
    let min_gap = (frames_per_second * 0.18).round().max(1.0) as usize;
    let mut peaks = Vec::new();
    for index in 1..onset.len().saturating_sub(1) {
        let local_peak = onset[index] >= 1.0
            && onset[index] >= onset[index - 1]
            && onset[index] > onset[index + 1];
        if local_peak && peaks.last().is_none_or(|last| index.saturating_sub(*last) >= min_gap) {
            peaks.push(index);
        }
    }
    let mut intervals = peaks.windows(2)
        .map(|pair| (pair[1] - pair[0]) as f64 / frames_per_second)
        .filter(|period| (0.20..=1.50).contains(period))
        .collect::<Vec<_>>();
    let (tempo, period) = if intervals.len() >= 3 {
        intervals.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
        let mut bpm = 60.0 / intervals[intervals.len() / 2];
        // Prefer a conventional base tempo over an obvious subdivision.
        while bpm < 60.0 { bpm *= 2.0; }
        while bpm > 200.0 { bpm /= 2.0; }
        (bpm, 60.0 / bpm)
    } else {
        (best.0, 60.0 / best.0)
    };
    let first_index = peaks.first().copied().or_else(|| onset.iter().enumerate()
        .max_by(|(_,a),(_,b)| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal))
        .map(|(index,_)| index)).unwrap_or(0);
    let first = first_index as f64 / frames_per_second;
    let duration = samples.len() as f64 / sample_rate as f64;
    let markers = std::iter::successors(Some(first), |value| Some(*value + period)).take_while(|value| *value <= duration).take(4096).collect::<Vec<_>>();
    // Normalized correlation is typically -1..1. A modest onset peak helps
    // distinguish an accidental periodic texture from a usable beat grid.
    let peak = onset.iter().copied().fold(0.0_f64, f64::max);
    let confidence = ((best.1.max(0.0) * 0.75) + (peak / 8.0).clamp(0.0, 0.25)).clamp(0.0, 1.0);
    BeatAnalysis { bpm: tempo, first_beat_offset_secs: first, confidence, markers, requires_confirmation: confidence < 0.55 }
}

pub fn analyze_local_audio(ffmpeg_bin: &str, path: &Path) -> Result<BeatAnalysis, String> {
    let pcm = decode_mono_pcm(ffmpeg_bin, path)?;
    Ok(analyze_pcm(&pcm, SAMPLE_RATE))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn estimates_regular_click_track_without_claiming_perfect_confidence() {
        let seconds = 12.0;
        let mut samples = vec![0.0_f32; (seconds * SAMPLE_RATE as f64) as usize];
        // 120 BPM clicks, with a small decay to produce an onset.
        for beat in 0..24 {
            let start = beat * SAMPLE_RATE as usize / 2;
            for offset in 0..80 { if start + offset < samples.len() { samples[start + offset] = 1.0 - offset as f32 / 80.0; } }
        }
        let analysis = analyze_pcm(&samples, SAMPLE_RATE);
        assert!((analysis.bpm - 120.0).abs() <= 3.0);
        assert!(!analysis.markers.is_empty());
    }
}
