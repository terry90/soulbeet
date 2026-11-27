//! Pure-Rust post-download audio verification gate.
//!
//! Runs container/duration inspection via `lofty`, packet spot-decoding and
//! interior digital-silence checks via `symphonia`, and spectral brickwall
//! fake-lossless detection via `rustfft` with zero external binary or C library
//! dependencies.

use lofty::file::{AudioFile, FileType, TaggedFileExt};
use lofty::probe::Probe;
use lofty::tag::Accessor;
use rustfft::num_complex::Complex;
use rustfft::FftPlanner;
use shared::failure::{Failure, FailureCode, RetryAdvice, Service, Stage};
use std::fs::File;
use std::io::Read;
use std::path::Path;
use symphonia::core::audio::SampleBuffer;
use symphonia::core::codecs::{Decoder, DecoderOptions, CODEC_TYPE_NULL};
use symphonia::core::errors::{Error as SymphoniaError, SeekErrorKind};
use symphonia::core::formats::{FormatOptions, FormatReader, SeekMode, SeekTo};
use symphonia::core::io::MediaSourceStream;
use symphonia::core::meta::MetadataOptions;
use symphonia::core::probe::Hint;
use symphonia::core::units::Time;

/// Verification strictness applied to a downloaded audio file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VerifyPolicy {
    /// Acquire & Play fast path: non-zero size, `lofty` container/duration,
    /// and start/middle/end packet spot-decode. No silence or spectral pass.
    Fast,
    /// Default downloads and discovery: `Fast` plus interior dead-silence check
    /// and conditional preview-truncation check.
    #[default]
    Standard,
    /// Archival FLAC upgrades only: `Standard` plus pure-Rust Welch FFT spectral
    /// brickwall fake-FLAC check on lossless files (>= 44.1 kHz).
    ArchivalLossless,
}

/// Summary metrics extracted during verification.
#[derive(Debug, Clone, PartialEq)]
pub struct VerifiedAudio {
    pub duration_secs: f64,
    pub sample_rate: Option<u32>,
    pub peak_dbfs: Option<f32>,
}

/// Amplitude threshold corresponding to -70 dBFS (`10^(-70 / 20)`).
const SILENCE_PEAK_THRESHOLD: f32 = 0.000_316_227_76;
/// Tracks shorter than 15s are exempt from dead-silence and spectral checks
/// (interludes, skits, sound effects).
const MIN_DURATION_FOR_INTERIOR_CHECKS_S: f64 = 15.0;
/// Preview-truncation check only engages when expected duration is at least 60s.
const MIN_EXPECTED_FOR_TRUNCATION_S: u32 = 60;
/// Minimum absolute threshold for preview-truncation detection (35s).
const PREVIEW_TRUNCATION_FLOOR_S: f64 = 35.0;
/// Relative threshold for preview-truncation detection (45% of expected duration).
const PREVIEW_TRUNCATION_RATIO: f64 = 0.45;
/// Welch FFT window size for spectral fake-lossless analysis.
const SPECTRAL_FFT_SIZE: usize = 4096;
/// Duration of each interior spectral analysis window in seconds.
const SPECTRAL_WINDOW_SECS: f64 = 2.0;
/// Maximum plausible audio sample rate accepted by verification (384 kHz DXD).
const MAX_PLAUSIBLE_SAMPLE_RATE: u32 = 384_000;
/// Hard upper bound on frames decoded into a single analysis window (3s at 384 kHz).
const MAX_DECODE_WINDOW_FRAMES: usize = (MAX_PLAUSIBLE_SAMPLE_RATE as usize) * 3;
/// Hard upper bound on packets inspected in a single decode pass.
const MAX_PACKETS_INSPECTED: usize = 4096;
/// Hard upper bound on decode errors tolerated in a single decode pass.
const MAX_DECODE_ERRORS: usize = 16;
/// Hard upper bound on decoder resets tolerated in a single decode pass.
const MAX_DECODE_RESETS: usize = 4;
/// Maximum audio channels retained per analysis window to bound memory usage.
const MAX_ANALYSIS_CHANNELS: usize = 8;
/// 16-byte ASF Header Object GUID (`75B22630-668E-11CF-A6D9-00AA0062CE6C`).
const ASF_HEADER_GUID: [u8; 16] = [
    0x30, 0x26, 0xB2, 0x75, 0x8E, 0x66, 0xCF, 0x11, 0xA6, 0xD9, 0x00, 0xAA, 0x00, 0x62, 0xCE, 0x6C,
];

fn verify_failure(code: FailureCode, message: impl Into<String>) -> Failure {
    Failure::new(code, Stage::Verify, RetryAdvice::OtherSource, message)
        .with_cause(Service::Soulful, None, "post-download audio verification")
}

fn validate_sample_rate(sample_rate: Option<u32>) -> Result<(), Failure> {
    if let Some(sr) = sample_rate {
        if sr == 0 || sr > MAX_PLAUSIBLE_SAMPLE_RATE {
            return Err(verify_failure(
                FailureCode::AudioCorrupt,
                format!("Downloaded audio declares implausible sample rate ({sr} Hz)"),
            ));
        }
    }
    Ok(())
}

/// Basic track ordering and duration metadata extracted via `lofty`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioTrackMeta {
    pub disc: Option<u32>,
    pub track: Option<u32>,
    pub duration_s: Option<u32>,
}

/// Reads an audio file's `(disc, track, duration_s)` metadata using `lofty`,
/// returning `None` when the file is unreadable or unsupported.
pub fn audio_track_meta(path: &Path) -> Option<AudioTrackMeta> {
    let tagged = probe_lofty(path).ok()?;
    let secs = tagged.properties().duration().as_secs_f64();
    let duration_s = if secs.is_finite() && secs >= 1.0 {
        Some(secs.round() as u32)
    } else {
        None
    };
    let tag = tagged.primary_tag().or_else(|| tagged.first_tag());
    let disc = tag.and_then(|t| t.disk()).filter(|&n| n > 0);
    let track = tag.and_then(|t| t.track()).filter(|&n| n > 0);
    Some(AudioTrackMeta {
        disc,
        track,
        duration_s,
    })
}

/// Reads an audio file's duration in whole seconds using `lofty`, returning
/// `None` when the file is unreadable, unsupported, or reports zero duration.
pub fn audio_duration_secs(path: &Path) -> Option<u32> {
    let tagged = probe_lofty(path).ok()?;
    let secs = tagged.properties().duration().as_secs_f64();
    if secs.is_finite() && secs >= 1.0 {
        Some(secs.round() as u32)
    } else {
        None
    }
}

fn probe_lofty(path: &Path) -> Result<lofty::file::TaggedFile, String> {
    let probe = Probe::open(path).map_err(|e| format!("open failed: {e}"))?;
    let probe = probe
        .guess_file_type()
        .map_err(|e| format!("file type probe failed: {e}"))?;
    probe
        .read()
        .map_err(|e| format!("container metadata read failed: {e}"))
}

fn is_wma_path(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("wma"))
}

fn verify_wma_asf_header(path: &Path) -> Result<VerifiedAudio, Failure> {
    let mut file = File::open(path).map_err(|e| {
        verify_failure(
            FailureCode::AudioCorrupt,
            format!("Downloaded WMA audio unreadable: {e}"),
        )
    })?;
    let mut header = [0u8; 16];
    file.read_exact(&mut header).map_err(|_| {
        verify_failure(
            FailureCode::AudioCorrupt,
            "Downloaded WMA file is truncated before ASF header",
        )
    })?;
    if header != ASF_HEADER_GUID {
        return Err(verify_failure(
            FailureCode::AudioCorrupt,
            "Downloaded WMA file does not start with a valid ASF header",
        ));
    }
    Ok(VerifiedAudio {
        duration_secs: 0.0,
        sample_rate: None,
        peak_dbfs: None,
    })
}

fn is_lofty_only_codec(file_type: FileType, path: &Path) -> bool {
    if matches!(file_type, FileType::Opus | FileType::Aac) {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("opus") || ext.eq_ignore_ascii_case("aac"))
}

fn is_lossless_for_spectral(file_type: FileType, path: &Path) -> bool {
    if matches!(file_type, FileType::Flac | FileType::Wav) {
        return true;
    }
    path.extension()
        .and_then(|e| e.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("flac") || ext.eq_ignore_ascii_case("wav"))
}

fn check_truncation(duration_secs: f64, expected_duration_s: Option<u32>) -> Result<(), Failure> {
    let Some(expected) = expected_duration_s else {
        return Ok(());
    };
    if expected < MIN_EXPECTED_FOR_TRUNCATION_S {
        return Ok(());
    }
    let cutoff = PREVIEW_TRUNCATION_FLOOR_S.max(expected as f64 * PREVIEW_TRUNCATION_RATIO);
    if duration_secs <= cutoff {
        return Err(verify_failure(
            FailureCode::AudioTruncated,
            format!(
                "Downloaded audio appears truncated ({:.0}s vs expected {}s)",
                duration_secs, expected
            ),
        ));
    }
    Ok(())
}

/// Verifies a downloaded audio file on disk according to `policy`.
///
/// Designed to be invoked inside `tokio::task::spawn_blocking`.
pub fn verify_downloaded_audio(
    path: &Path,
    policy: VerifyPolicy,
    expected_duration_s: Option<u32>,
) -> Result<VerifiedAudio, Failure> {
    let meta = std::fs::metadata(path).map_err(|e| {
        verify_failure(
            FailureCode::AudioCorrupt,
            format!("Downloaded audio unreadable: {e}"),
        )
    })?;
    if meta.len() == 0 {
        return Err(verify_failure(
            FailureCode::AudioCorrupt,
            "Downloaded audio file is empty (0 bytes)",
        ));
    }

    // WMA (ASF container) is not supported by lofty or pure-Rust symphonia;
    // validate non-zero size and the 16-byte ASF Header Object GUID before probing.
    if is_wma_path(path) {
        return verify_wma_asf_header(path);
    }

    let tagged = probe_lofty(path).map_err(|e| {
        verify_failure(
            FailureCode::AudioCorrupt,
            format!("Downloaded audio container is corrupt or unreadable: {e}"),
        )
    })?;
    let file_type = tagged.file_type();
    let props = tagged.properties();
    let duration_secs = props.duration().as_secs_f64();
    if !duration_secs.is_finite() || duration_secs <= 0.0 {
        return Err(verify_failure(
            FailureCode::AudioCorrupt,
            "Downloaded audio reports zero duration",
        ));
    }
    let lofty_sample_rate = props.sample_rate();
    validate_sample_rate(lofty_sample_rate)?;

    if policy != VerifyPolicy::Fast {
        check_truncation(duration_secs, expected_duration_s)?;
    }

    // Opus and raw ADTS AAC are validated via lofty container + non-zero duration
    // (+ truncation check above).
    if is_lofty_only_codec(file_type, path) {
        return Ok(VerifiedAudio {
            duration_secs,
            sample_rate: lofty_sample_rate,
            peak_dbfs: None,
        });
    }

    let mut stream = open_symphonia(path, lofty_sample_rate)?;
    validate_sample_rate(stream.sample_rate)?;

    // Fast spot-decode: verify start, middle, and end packets decode cleanly.
    spot_decode(&mut *stream.format, &mut *stream.decoder, stream.track_id, duration_secs)?;

    let mut peak_dbfs = None;
    if policy != VerifyPolicy::Fast && duration_secs >= MIN_DURATION_FOR_INTERIOR_CHECKS_S {
        let peak_abs = check_interior_silence(
            &mut *stream.format,
            &mut *stream.decoder,
            stream.track_id,
            duration_secs,
            stream.sample_rate,
        )?;
        peak_dbfs = Some(if peak_abs > 0.0 {
            20.0 * peak_abs.log10()
        } else {
            -160.0
        });
    }

    if policy == VerifyPolicy::ArchivalLossless
        && duration_secs >= MIN_DURATION_FOR_INTERIOR_CHECKS_S
        && is_lossless_for_spectral(file_type, path)
        && stream.sample_rate.is_some_and(|sr| sr >= 44_100)
    {
        let sr = stream.sample_rate.unwrap();
        check_spectral_brickwall(
            &mut *stream.format,
            &mut *stream.decoder,
            stream.track_id,
            duration_secs,
            sr,
        )?;
    }

    Ok(VerifiedAudio {
        duration_secs,
        sample_rate: stream.sample_rate,
        peak_dbfs,
    })
}

struct OpenedStream {
    format: Box<dyn FormatReader>,
    decoder: Box<dyn Decoder>,
    track_id: u32,
    sample_rate: Option<u32>,
}

fn open_symphonia(path: &Path, fallback_sr: Option<u32>) -> Result<OpenedStream, Failure> {
    let probe_with_hint = |use_hint: bool| -> Result<Box<dyn FormatReader>, SymphoniaError> {
        let file = File::open(path).map_err(SymphoniaError::IoError)?;
        let mss = MediaSourceStream::new(Box::new(file), Default::default());
        let mut hint = Hint::new();
        if use_hint {
            if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                hint.with_extension(ext);
            }
        }
        let probed = symphonia::default::get_probe().format(
            &hint,
            mss,
            &FormatOptions::default(),
            &MetadataOptions::default(),
        )?;
        Ok(probed.format)
    };

    let format = probe_with_hint(true)
        .or_else(|_| probe_with_hint(false))
        .map_err(|e| {
            verify_failure(
                FailureCode::AudioCorrupt,
                format!("Audio stream could not be demuxed: {e}"),
            )
        })?;

    let track = format
        .default_track()
        .filter(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        .or_else(|| {
            format
                .tracks()
                .iter()
                .find(|t| t.codec_params.codec != CODEC_TYPE_NULL)
        })
        .ok_or_else(|| {
            verify_failure(
                FailureCode::AudioCorrupt,
                "Audio file contains no decodable audio track",
            )
        })?;

    let track_id = track.id;
    let sample_rate = track.codec_params.sample_rate.or(fallback_sr);
    let decoder = symphonia::default::get_codecs()
        .make(&track.codec_params, &DecoderOptions::default())
        .map_err(|e| {
            verify_failure(
                FailureCode::AudioCorrupt,
                format!("Audio codec decoder initialization failed: {e}"),
            )
        })?;

    Ok(OpenedStream {
        format,
        decoder,
        track_id,
        sample_rate,
    })
}

/// Decodes up to `max_packets` packets from the current stream position,
/// returning how many packets decoded cleanly and the peak absolute amplitude seen.
fn decode_n_packets(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    max_packets: usize,
) -> Result<(usize, f32), Failure> {
    let mut decoded_packets = 0usize;
    let mut packets_inspected = 0usize;
    let mut decode_errors = 0usize;
    let mut resets = 0usize;
    let mut peak = 0.0_f32;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    while decoded_packets < max_packets {
        packets_inspected += 1;
        if packets_inspected > MAX_PACKETS_INSPECTED {
            return Err(verify_failure(
                FailureCode::AudioCorrupt,
                "Audio stream exceeded packet inspection limit",
            ));
        }
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                resets += 1;
                if resets > MAX_DECODE_RESETS {
                    return Err(verify_failure(
                        FailureCode::AudioCorrupt,
                        "Audio stream triggered repeated decoder resets",
                    ));
                }
                decoder.reset();
                continue;
            }
            Err(e) => {
                return Err(verify_failure(
                    FailureCode::AudioCorrupt,
                    format!("Corrupt audio stream packet: {e}"),
                ));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = *audio_buf.spec();
                let frames = audio_buf.capacity() as u64;
                let buf = match &mut sample_buf {
                    Some(b) if b.capacity() >= audio_buf.capacity() * spec.channels.count() => b,
                    slot => slot.insert(SampleBuffer::<f32>::new(frames, spec)),
                };
                buf.copy_interleaved_ref(audio_buf);
                for &s in buf.samples() {
                    let a = s.abs();
                    if a > peak {
                        peak = a;
                    }
                }
                decoded_packets += 1;
            }
            Err(SymphoniaError::DecodeError(_)) => {
                decode_errors += 1;
                if (decode_errors >= 4 && decoded_packets == 0) || decode_errors > MAX_DECODE_ERRORS
                {
                    return Err(verify_failure(
                        FailureCode::AudioCorrupt,
                        "Audio frames failed decoding repeatedly",
                    ));
                }
            }
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                resets += 1;
                if resets > MAX_DECODE_RESETS {
                    return Err(verify_failure(
                        FailureCode::AudioCorrupt,
                        "Audio stream triggered repeated decoder resets",
                    ));
                }
                decoder.reset();
            }
            Err(e) => {
                return Err(verify_failure(
                    FailureCode::AudioCorrupt,
                    format!("Fatal audio decode error: {e}"),
                ));
            }
        }
    }

    Ok((decoded_packets, peak))
}

enum SeekOutcome {
    Seeked,
    Unseekable,
    Failed(Failure),
}

fn seek_to_secs(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    secs: f64,
) -> SeekOutcome {
    let clamped = secs.max(0.0);
    let whole = clamped.trunc() as u64;
    let frac = clamped.fract();
    match format.seek(
        SeekMode::Coarse,
        SeekTo::Time {
            time: Time::new(whole, frac),
            track_id: Some(track_id),
        },
    ) {
        Ok(_) => {
            decoder.reset();
            SeekOutcome::Seeked
        }
        Err(SymphoniaError::SeekError(SeekErrorKind::Unseekable))
        | Err(SymphoniaError::Unsupported(_)) => SeekOutcome::Unseekable,
        Err(SymphoniaError::SeekError(SeekErrorKind::OutOfRange)) => SeekOutcome::Failed(
            verify_failure(
                FailureCode::AudioCorrupt,
                format!("Audio stream ends before declared timestamp ({secs:.1}s)"),
            ),
        ),
        Err(e) => SeekOutcome::Failed(verify_failure(
            FailureCode::AudioCorrupt,
            format!("Seek failed at {secs:.1}s: {e}"),
        )),
    }
}

fn spot_decode(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    duration_secs: f64,
) -> Result<(), Failure> {
    let (start_ok, _) = decode_n_packets(format, decoder, track_id, 3)?;
    if start_ok == 0 {
        return Err(verify_failure(
            FailureCode::AudioCorrupt,
            "No decodable audio frames found at start of file",
        ));
    }

    if duration_secs < 2.0 {
        return Ok(());
    }

    // Middle spot-check (~50%)
    match seek_to_secs(format, decoder, track_id, duration_secs * 0.5) {
        SeekOutcome::Seeked => {
            let (mid_ok, _) = decode_n_packets(format, decoder, track_id, 2)?;
            if mid_ok == 0 {
                return Err(verify_failure(
                    FailureCode::AudioCorrupt,
                    "Audio file is truncated or unreadable at midpoint",
                ));
            }
        }
        SeekOutcome::Unseekable => {
            // Format doesn't support seeking; decode a few more sequential packets to confirm health.
            let _ = decode_n_packets(format, decoder, track_id, 5)?;
            return Ok(());
        }
        SeekOutcome::Failed(f) => return Err(f),
    }

    // End spot-check (~85% or 1.5s before declared end, for files >= 3s)
    if duration_secs >= 3.0 {
        let end_target = (duration_secs * 0.85).min((duration_secs - 1.5).max(0.5));
        match seek_to_secs(format, decoder, track_id, end_target) {
            SeekOutcome::Seeked => {
                let (end_ok, _) = decode_n_packets(format, decoder, track_id, 2)?;
                if end_ok == 0 {
                    return Err(verify_failure(
                        FailureCode::AudioCorrupt,
                        "Audio file is truncated or unreadable near end",
                    ));
                }
            }
            SeekOutcome::Unseekable => {}
            SeekOutcome::Failed(f) => return Err(f),
        }
    }

    Ok(())
}

/// Decodes up to `target_frames` per-channel samples starting at the current
/// stream position, returning deinterleaved channel buffers and the peak
/// absolute value across all channels.
fn decode_channel_window(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    target_frames: usize,
) -> Result<(Vec<Vec<f32>>, f32), Failure> {
    let target_frames = target_frames.min(MAX_DECODE_WINDOW_FRAMES);
    let mut channel_samples: Vec<Vec<f32>> = Vec::new();
    let mut frames_decoded = 0usize;
    let mut packets_inspected = 0usize;
    let mut decode_errors = 0usize;
    let mut resets = 0usize;
    let mut peak = 0.0_f32;
    let mut sample_buf: Option<SampleBuffer<f32>> = None;

    while frames_decoded < target_frames {
        packets_inspected += 1;
        if packets_inspected > MAX_PACKETS_INSPECTED {
            if frames_decoded == 0 {
                return Err(verify_failure(
                    FailureCode::AudioCorrupt,
                    "Audio stream exceeded packet inspection limit during analysis",
                ));
            }
            break;
        }
        let packet = match format.next_packet() {
            Ok(p) => p,
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                resets += 1;
                if resets > MAX_DECODE_RESETS {
                    return Err(verify_failure(
                        FailureCode::AudioCorrupt,
                        "Audio stream triggered repeated decoder resets during analysis",
                    ));
                }
                decoder.reset();
                continue;
            }
            Err(e) => {
                return Err(verify_failure(
                    FailureCode::AudioCorrupt,
                    format!("Corrupt audio stream during analysis: {e}"),
                ));
            }
        };
        if packet.track_id() != track_id {
            continue;
        }
        match decoder.decode(&packet) {
            Ok(audio_buf) => {
                let spec = *audio_buf.spec();
                let raw_channels = spec.channels.count().max(1);
                let kept_channels = raw_channels.min(MAX_ANALYSIS_CHANNELS);
                let frames = audio_buf.capacity() as u64;
                let buf = match &mut sample_buf {
                    Some(b) if b.capacity() >= audio_buf.capacity() * raw_channels => b,
                    slot => slot.insert(SampleBuffer::<f32>::new(frames, spec)),
                };
                buf.copy_interleaved_ref(audio_buf);
                if channel_samples.is_empty() {
                    channel_samples = (0..kept_channels)
                        .map(|_| Vec::with_capacity(target_frames))
                        .collect();
                }
                for frame in buf.samples().chunks_exact(raw_channels) {
                    for &s in frame {
                        let a = s.abs();
                        if a > peak {
                            peak = a;
                        }
                    }
                    for (ch_buf, &s) in channel_samples.iter_mut().zip(frame.iter()) {
                        ch_buf.push(s);
                    }
                    frames_decoded += 1;
                    if frames_decoded >= target_frames {
                        break;
                    }
                }
            }
            Err(SymphoniaError::DecodeError(_)) => {
                decode_errors += 1;
                if decode_errors > MAX_DECODE_ERRORS {
                    return Err(verify_failure(
                        FailureCode::AudioCorrupt,
                        "Audio frames failed decoding repeatedly during analysis",
                    ));
                }
            }
            Err(SymphoniaError::IoError(e)) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                break;
            }
            Err(SymphoniaError::ResetRequired) => {
                resets += 1;
                if resets > MAX_DECODE_RESETS {
                    return Err(verify_failure(
                        FailureCode::AudioCorrupt,
                        "Audio stream triggered repeated decoder resets during analysis",
                    ));
                }
                decoder.reset();
            }
            Err(e) => {
                return Err(verify_failure(
                    FailureCode::AudioCorrupt,
                    format!("Audio decode error during analysis: {e}"),
                ));
            }
        }
    }

    Ok((channel_samples, peak))
}

/// Checks 5 interior slices (15%, 30%, 50%, 70%, 85%) for dead digital silence
/// (`peak <= -70 dBFS`), ignoring leading/trailing silence.
fn check_interior_silence(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    duration_secs: f64,
    sample_rate: Option<u32>,
) -> Result<f32, Failure> {
    let sr = sample_rate
        .unwrap_or(44_100)
        .min(MAX_PLAUSIBLE_SAMPLE_RATE) as usize;
    let slice_frames = (sr / 2).max(2048); // ~0.5s per interior slice
    let positions = [0.15, 0.30, 0.50, 0.70, 0.85];
    let mut max_peak = 0.0_f32;
    let mut total_samples = 0usize;

    for frac in positions {
        let target_s = duration_secs * frac;
        match seek_to_secs(format, decoder, track_id, target_s) {
            SeekOutcome::Seeked => {
                let (channels, peak) =
                    decode_channel_window(format, decoder, track_id, slice_frames)?;
                total_samples += channels.first().map_or(0, Vec::len);
                if peak > max_peak {
                    max_peak = peak;
                }
                if max_peak > SILENCE_PEAK_THRESHOLD {
                    // Already clearly audible; no need to inspect remaining slices.
                    return Ok(max_peak);
                }
            }
            SeekOutcome::Unseekable => {
                // Fall back to checking what we can decode sequentially without failing valid streams.
                return Ok(1.0);
            }
            SeekOutcome::Failed(f) => return Err(f),
        }
    }

    if total_samples > 0 && max_peak <= SILENCE_PEAK_THRESHOLD {
        return Err(verify_failure(
            FailureCode::AudioSilent,
            "Downloaded audio is digitally silent across all interior slices",
        ));
    }

    Ok(max_peak)
}

/// Result of spectral brickwall analysis on a single 2-second interior window.
enum WindowSpectralVerdict {
    /// Window had too little mid-band energy (quiet passage / breakdown) to judge.
    TooQuiet,
    /// Window shows genuine upper-band energy or gradual natural rolloff.
    Genuine,
    /// Window shows strong mid-band content, a sharp spectral cliff at <= 16.5 kHz,
    /// and a dead flat floor above 18.5 kHz.
    Brickwall { cutoff_hz: f64 },
}

/// Evaluates Welch-averaged power spectral density per channel (averaged across
/// channels per frequency bin) to detect a high-confidence lossy brickwall
/// lowpass cutoff without out-of-phase stereo cancellation.
fn analyze_window_spectrum(channels: &[Vec<f32>], sample_rate: u32) -> WindowSpectralVerdict {
    if sample_rate < 44_100 {
        return WindowSpectralVerdict::TooQuiet;
    }

    let n = SPECTRAL_FFT_SIZE;
    let hop = n / 2;
    let num_bins = n / 2 + 1;

    let usable: Vec<&[f32]> = channels
        .iter()
        .map(Vec::as_slice)
        .filter(|ch| ch.len() >= n)
        .collect();
    if usable.is_empty() {
        return WindowSpectralVerdict::TooQuiet;
    }

    // Precompute Hann window and its power normalization.
    let mut hann = Vec::with_capacity(n);
    let mut win_power = 0.0_f64;
    for i in 0..n {
        let w = 0.5 * (1.0 - (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos());
        win_power += w * w;
        hann.push(w as f32);
    }

    let mut planner = FftPlanner::<f32>::new();
    let fft = planner.plan_fft_forward(n);
    let mut psd = vec![0.0_f64; num_bins];
    let mut ch_psd = vec![0.0_f64; num_bins];
    let mut frame_buf = vec![Complex { re: 0.0_f32, im: 0.0_f32 }; n];
    let mut active_channels = 0usize;

    for samples in usable {
        ch_psd.fill(0.0);
        let mut frame_count = 0usize;
        let mut offset = 0usize;
        while offset + n <= samples.len() {
            for i in 0..n {
                frame_buf[i] = Complex {
                    re: samples[offset + i] * hann[i],
                    im: 0.0,
                };
            }
            fft.process(&mut frame_buf);
            // One-sided Parseval-calibrated power per bin: sum_k psd[k] == mean(x^2).
            for k in 0..num_bins {
                let re = frame_buf[k].re as f64;
                let im = frame_buf[k].im as f64;
                let scale = if k == 0 || k == num_bins - 1 { 1.0 } else { 2.0 };
                ch_psd[k] += scale * (re * re + im * im) / (win_power * n as f64);
            }
            frame_count += 1;
            offset += hop;
        }
        if frame_count > 0 {
            let inv_frames = 1.0 / frame_count as f64;
            for (acc, &val) in psd.iter_mut().zip(&ch_psd) {
                *acc += val * inv_frames;
            }
            active_channels += 1;
        }
    }

    if active_channels == 0 {
        return WindowSpectralVerdict::TooQuiet;
    }
    let inv_channels = 1.0 / active_channels as f64;
    for p in &mut psd {
        *p *= inv_channels;
    }

    let bin_hz = sample_rate as f64 / n as f64;
    // Bandwidth-normalized spectral power density in dBFS/kHz.
    let band_db = |lo_hz: f64, hi_hz: f64| -> f64 {
        let lo = ((lo_hz / bin_hz).floor() as usize).clamp(1, num_bins - 1);
        let hi = ((hi_hz / bin_hz).ceil() as usize).clamp(lo + 1, num_bins);
        let band_power: f64 = psd[lo..hi].iter().sum();
        let width_khz = ((hi - lo) as f64 * bin_hz / 1000.0).max(0.001);
        10.0 * (band_power / width_khz + 1e-20).log10()
    };

    // 1. Require real mid-band energy (2..10 kHz) so quiet ambient/breakdown passages
    //    are never misclassified.
    let mid_db = band_db(2_000.0, 10_000.0);
    if mid_db < -65.0 {
        return WindowSpectralVerdict::TooQuiet;
    }

    // 2. High shelf above 18.5 kHz (capped cleanly below Nyquist) must be a dead flat floor.
    let high_hi = 21_000.0_f64.min(sample_rate as f64 * 0.5 - 500.0);
    if high_hi <= 19_000.0 {
        return WindowSpectralVerdict::Genuine;
    }
    let high_shelf_db = band_db(18_500.0, high_hi);
    // Also check sub-bands above 18.5 kHz so real sparse cymbals/harmonics aren't averaged away.
    let high_sub_a = band_db(18_500.0, 19_500.0);
    let high_sub_b = band_db(19_500.0, high_hi);
    if high_shelf_db > -82.0 || high_sub_a > -78.0 || high_sub_b > -78.0 {
        return WindowSpectralVerdict::Genuine;
    }

    // 3. Scan candidate brickwall cutoffs in 11.0..=16.5 kHz (step 500 Hz).
    //    A lossy transcode has real energy right below the cutoff and drops sharply
    //    within a narrow ~2 kHz transition band into a dead floor.
    let mut best_cliff: Option<(f64, f64)> = None;
    let mut fc = 11_000.0_f64;
    while fc <= 16_500.0 {
        let below_db = band_db(fc - 1_500.0, fc);
        let above_db = band_db(fc + 1_000.0, fc + 2_500.0);
        let drop_db = below_db - above_db;
        // Must have substantial energy just below the cutoff (not already rolled off
        // like vintage mono / dark acoustic / lo-fi vinyl) AND a sharp >= 25 dB cliff
        // AND at least 28 dB below-cutoff-to-high-shelf drop.
        let has_pre_cliff_energy = below_db > -72.0 && (mid_db - below_db) <= 30.0;
        let dead_above = above_db <= -80.0 && (below_db - high_shelf_db) >= 28.0;
        if has_pre_cliff_energy && dead_above && drop_db >= 25.0 {
            match best_cliff {
                Some((_, best_drop)) if drop_db <= best_drop => {}
                _ => best_cliff = Some((fc, drop_db)),
            }
        }
        fc += 500.0;
    }

    match best_cliff {
        Some((cutoff_hz, _)) => WindowSpectralVerdict::Brickwall { cutoff_hz },
        None => WindowSpectralVerdict::Genuine,
    }
}

/// Checks 3-5 interior 2-second windows of a lossless file (>= 44.1 kHz) for a
/// high-confidence sharp brickwall lossy transcode.
fn check_spectral_brickwall(
    format: &mut dyn FormatReader,
    decoder: &mut dyn Decoder,
    track_id: u32,
    duration_secs: f64,
    sample_rate: u32,
) -> Result<(), Failure> {
    let positions: &[f64] = if duration_secs >= 30.0 {
        &[0.20, 0.35, 0.50, 0.65, 0.80]
    } else {
        &[0.25, 0.50, 0.75]
    };
    let window_frames = (sample_rate as f64 * SPECTRAL_WINDOW_SECS).round() as usize;

    let mut active_windows = 0usize;
    let mut brickwall_windows = 0usize;
    let mut last_cutoff_hz = 0.0_f64;

    for &frac in positions {
        let target_s = (duration_secs * frac).min((duration_secs - SPECTRAL_WINDOW_SECS).max(0.0));
        match seek_to_secs(format, decoder, track_id, target_s) {
            SeekOutcome::Seeked => {
                let (channels, _) =
                    decode_channel_window(format, decoder, track_id, window_frames)?;
                match analyze_window_spectrum(&channels, sample_rate) {
                    WindowSpectralVerdict::TooQuiet => {}
                    WindowSpectralVerdict::Genuine => {
                        active_windows += 1;
                    }
                    WindowSpectralVerdict::Brickwall { cutoff_hz } => {
                        active_windows += 1;
                        brickwall_windows += 1;
                        last_cutoff_hz = cutoff_hz;
                    }
                }
            }
            SeekOutcome::Unseekable => return Ok(()),
            SeekOutcome::Failed(f) => return Err(f),
        }
    }

    // Require consensus across at least 2 active windows (>= 75% flagged) so a
    // single unusual passage never causes a false positive.
    if active_windows >= 2 && brickwall_windows * 4 >= active_windows * 3 {
        return Err(verify_failure(
            FailureCode::AudioFakeLossless,
            format!(
                "Lossless file failed spectral verification: sharp brickwall cutoff near {:.1} kHz across {}/{} windows",
                last_cutoff_hz / 1000.0,
                brickwall_windows,
                active_windows
            ),
        ));
    }

    Ok(())
}

/// Builds a minimal spec-compliant uncompressed 16-bit PCM WAV byte vector
/// from mono `samples` in `-1.0..=1.0`. Useful in tests across the workspace.
pub fn encode_test_wav_pcm16(sample_rate: u32, samples: &[f32]) -> Vec<u8> {
    let data_bytes = (samples.len() * 2) as u32;
    let riff_size = 36 + data_bytes;
    let mut out = Vec::with_capacity(44 + data_bytes as usize);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&riff_size.to_le_bytes());
    out.extend_from_slice(b"WAVE");
    out.extend_from_slice(b"fmt ");
    out.extend_from_slice(&16u32.to_le_bytes()); // PCM chunk size
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM format
    out.extend_from_slice(&1u16.to_le_bytes()); // mono
    out.extend_from_slice(&sample_rate.to_le_bytes());
    out.extend_from_slice(&(sample_rate * 2).to_le_bytes()); // byte rate
    out.extend_from_slice(&2u16.to_le_bytes()); // block align
    out.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_bytes.to_le_bytes());
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let q = (clamped * i16::MAX as f32).round() as i16;
        out.extend_from_slice(&q.to_le_bytes());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::f32::consts::TAU;

    fn write_wav(dir: &tempfile::TempDir, name: &str, sample_rate: u32, samples: &[f32]) -> std::path::PathBuf {
        let path = dir.path().join(name);
        std::fs::write(&path, encode_test_wav_pcm16(sample_rate, samples)).unwrap();
        path
    }

    /// Deterministic full-spectrum signal (multi-tone harmonics + broadband noise)
    /// spanning 200 Hz up to 21 kHz.
    fn full_spectrum_signal(sample_rate: u32, duration_s: usize) -> Vec<f32> {
        let total = sample_rate as usize * duration_s;
        let mut out = Vec::with_capacity(total);
        let mut seed: u64 = 0x1234_5678_9abc_def0;
        for i in 0..total {
            // Simple LCG broadband noise + harmonic tones
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let noise = (((seed >> 33) as f32) / (u32::MAX as f32) - 0.5) * 0.4;
            let t = i as f32 / sample_rate as f32;
            let tone = 0.25 * (TAU * 440.0 * t).sin() + 0.15 * (TAU * 4000.0 * t).sin();
            out.push((noise + tone).clamp(-0.95, 0.95));
        }
        out
    }

    /// Applies an ideal frequency-domain brickwall lowpass filter at `cutoff_hz`
    /// (mimicking a 128/192 kbps MP3 transcode).
    fn brickwall_lowpass(samples: &[f32], sample_rate: u32, cutoff_hz: f32) -> Vec<f32> {
        let n = samples.len().next_power_of_two();
        let mut planner = FftPlanner::<f32>::new();
        let fwd = planner.plan_fft_forward(n);
        let inv = planner.plan_fft_inverse(n);
        let mut buf: Vec<Complex<f32>> = samples
            .iter()
            .map(|&re| Complex { re, im: 0.0 })
            .chain(std::iter::repeat_n(Complex { re: 0.0, im: 0.0 }, n - samples.len()))
            .collect();
        fwd.process(&mut buf);
        let bin_hz = sample_rate as f32 / n as f32;
        for (k, c) in buf.iter_mut().enumerate() {
            let freq = if k <= n / 2 {
                k as f32 * bin_hz
            } else {
                (n - k) as f32 * bin_hz
            };
            if freq > cutoff_hz {
                *c = Complex { re: 0.0, im: 0.0 };
            }
        }
        inv.process(&mut buf);
        let scale = 1.0 / n as f32;
        buf[..samples.len()].iter().map(|c| c.re * scale).collect()
    }

    /// Applies a gentle 1st-order analog RC lowpass filter (6 dB/octave), like a
    /// warm vintage mono/lo-fi recording, which must NOT trigger brickwall detection.
    fn gentle_analog_rolloff(samples: &[f32], sample_rate: u32, rc_cutoff_hz: f32) -> Vec<f32> {
        let dt = 1.0 / sample_rate as f32;
        let rc = 1.0 / (TAU * rc_cutoff_hz);
        let alpha = dt / (rc + dt);
        let mut out = Vec::with_capacity(samples.len());
        let mut prev = 0.0_f32;
        for &x in samples {
            prev += alpha * (x - prev);
            out.push(prev);
        }
        out
    }

    #[test]
    fn valid_wav_passes_fast_standard_and_archival_policies() {
        let dir = tempfile::tempdir().unwrap();
        let signal = full_spectrum_signal(44_100, 18);
        let path = write_wav(&dir, "full.wav", 44_100, &signal);

        for policy in [
            VerifyPolicy::Fast,
            VerifyPolicy::Standard,
            VerifyPolicy::ArchivalLossless,
        ] {
            let verified = verify_downloaded_audio(&path, policy, Some(18)).expect("valid wav");
            assert!((verified.duration_secs - 18.0).abs() < 0.5);
            assert_eq!(verified.sample_rate, Some(44_100));
        }
        assert_eq!(audio_duration_secs(&path), Some(18));
    }

    #[test]
    fn empty_and_corrupt_files_fail_with_audio_corrupt() {
        let dir = tempfile::tempdir().unwrap();
        let empty = dir.path().join("empty.flac");
        std::fs::write(&empty, b"").unwrap();
        let err = verify_downloaded_audio(&empty, VerifyPolicy::Fast, None).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioCorrupt);
        assert_eq!(err.stage, Stage::Verify);

        let garbage = dir.path().join("garbage.flac");
        std::fs::write(&garbage, b"<html>404 Not Found</html>").unwrap();
        let err = verify_downloaded_audio(&garbage, VerifyPolicy::Fast, None).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioCorrupt);
    }

    #[test]
    fn dead_digital_silence_fails_standard_but_exempts_short_tracks_and_fast_policy() {
        let dir = tempfile::tempdir().unwrap();
        let silent_20s = vec![0.0_f32; 44_100 * 20];
        let path_20s = write_wav(&dir, "silent20.wav", 44_100, &silent_20s);

        // Fast policy skips silence check.
        assert!(verify_downloaded_audio(&path_20s, VerifyPolicy::Fast, None).is_ok());
        // Standard policy flags dead digital silence.
        let err = verify_downloaded_audio(&path_20s, VerifyPolicy::Standard, None).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioSilent);
        assert_eq!(err.stage, Stage::Verify);

        // Short tracks (< 15s) are exempt from silence detection.
        let silent_10s = vec![0.0_f32; 44_100 * 10];
        let path_10s = write_wav(&dir, "silent10.wav", 44_100, &silent_10s);
        assert!(verify_downloaded_audio(&path_10s, VerifyPolicy::Standard, None).is_ok());

        // Track with leading and trailing silence but audible interior passes.
        let mut padded = vec![0.0_f32; 44_100 * 20];
        for (i, sample) in padded[44_100 * 8..44_100 * 12].iter_mut().enumerate() {
            *sample = 0.2 * (TAU * 440.0 * i as f32 / 44_100.0).sin();
        }
        let path_padded = write_wav(&dir, "padded.wav", 44_100, &padded);
        assert!(verify_downloaded_audio(&path_padded, VerifyPolicy::Standard, None).is_ok());
    }

    #[test]
    fn preview_truncation_fails_standard_only_when_expected_is_long_enough() {
        let dir = tempfile::tempdir().unwrap();
        let clip_30s = full_spectrum_signal(44_100, 30);
        let path = write_wav(&dir, "preview30.wav", 44_100, &clip_30s);

        // 30s file when 240s expected -> flagged as AudioTruncated under Standard.
        let err = verify_downloaded_audio(&path, VerifyPolicy::Standard, Some(240)).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioTruncated);
        assert_eq!(err.stage, Stage::Verify);

        // Fast policy skips truncation check.
        assert!(verify_downloaded_audio(&path, VerifyPolicy::Fast, Some(240)).is_ok());

        // When expected duration is unknown or < 60s, no truncation failure.
        assert!(verify_downloaded_audio(&path, VerifyPolicy::Standard, None).is_ok());
        assert!(verify_downloaded_audio(&path, VerifyPolicy::Standard, Some(45)).is_ok());

        // When actual is > 45% of expected (e.g., 30s vs 62s -> cutoff 35s, 30 <= 35 still fails;
        // 40s vs 70s -> cutoff max(35, 31.5) = 35 -> passes).
        let clip_40s = full_spectrum_signal(44_100, 40);
        let path_40s = write_wav(&dir, "clip40.wav", 44_100, &clip_40s);
        assert!(verify_downloaded_audio(&path_40s, VerifyPolicy::Standard, Some(70)).is_ok());
    }

    #[test]
    fn brickwall_lowpass_pcm_is_flagged_by_archival_lossless_only() {
        let dir = tempfile::tempdir().unwrap();
        let full = full_spectrum_signal(44_100, 18);
        let brickwalled = brickwall_lowpass(&full, 44_100, 15_500.0);
        let path = write_wav(&dir, "fake_flac.wav", 44_100, &brickwalled);

        // Standard does NOT run spectral brickwall check.
        assert!(verify_downloaded_audio(&path, VerifyPolicy::Standard, None).is_ok());

        // ArchivalLossless flags the sharp brickwall cutoff.
        let err =
            verify_downloaded_audio(&path, VerifyPolicy::ArchivalLossless, None).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioFakeLossless);
        assert_eq!(err.stage, Stage::Verify);
    }

    #[test]
    fn gentle_lofi_rolloff_does_not_false_positive_as_fake_lossless() {
        let dir = tempfile::tempdir().unwrap();
        let full = full_spectrum_signal(44_100, 18);
        let lofi = gentle_analog_rolloff(&full, 44_100, 4_000.0);
        let path = write_wav(&dir, "vintage_lofi.wav", 44_100, &lofi);

        assert!(verify_downloaded_audio(&path, VerifyPolicy::ArchivalLossless, None).is_ok());
    }

    #[test]
    fn implausible_sample_rate_fails_as_audio_corrupt_without_oom() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("crafted_sr.wav");
        let data_bytes: u32 = 320_000;
        let riff_size: u32 = 36 + data_bytes;
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_size.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
        wav.extend_from_slice(&1u16.to_le_bytes()); // mono
        wav.extend_from_slice(&u32::MAX.to_le_bytes()); // implausible sample rate
        wav.extend_from_slice(&16_000u32.to_le_bytes()); // byte rate -> 20s declared duration
        wav.extend_from_slice(&2u16.to_le_bytes()); // block align
        wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        wav.resize(44 + data_bytes as usize, 0);
        std::fs::write(&path, wav).unwrap();

        let err = verify_downloaded_audio(&path, VerifyPolicy::Standard, None).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioCorrupt);
        assert_eq!(err.stage, Stage::Verify);
    }

    #[test]
    fn valid_asf_wma_passes_and_corrupt_wma_fails_as_audio_corrupt() {
        let dir = tempfile::tempdir().unwrap();

        let valid_wma = dir.path().join("valid.wma");
        let mut asf_bytes = ASF_HEADER_GUID.to_vec();
        asf_bytes.extend_from_slice(&[0u8; 64]);
        std::fs::write(&valid_wma, asf_bytes).unwrap();
        assert!(verify_downloaded_audio(&valid_wma, VerifyPolicy::Standard, Some(180)).is_ok());

        let corrupt_wma = dir.path().join("corrupt.wma");
        std::fs::write(&corrupt_wma, b"not an asf container at all").unwrap();
        let err = verify_downloaded_audio(&corrupt_wma, VerifyPolicy::Standard, None).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioCorrupt);
        assert_eq!(err.stage, Stage::Verify);

        let short_wma = dir.path().join("short.wma");
        std::fs::write(&short_wma, &ASF_HEADER_GUID[..8]).unwrap();
        let err = verify_downloaded_audio(&short_wma, VerifyPolicy::Fast, None).unwrap_err();
        assert_eq!(err.code, FailureCode::AudioCorrupt);
        assert_eq!(err.stage, Stage::Verify);
    }

    #[test]
    fn out_of_phase_stereo_high_frequencies_do_not_false_positive() {
        // Build a stereo signal where mid-band (440 Hz + 14 kHz) is in-phase mono,
        // while high-band (19.5 kHz) is strictly out-of-phase (L = +h, R = -h).
        // Naive (L + R) / 2 mono downmix cancels the 19.5 kHz content completely
        // and would falsely flag a brickwall; per-channel PSD preserves it.
        let sr = 44_100u32;
        let total = sr as usize * 18;
        let full = full_spectrum_signal(sr, 18);
        let low = brickwall_lowpass(&full, sr, 15_500.0);
        let mut left = Vec::with_capacity(total);
        let mut right = Vec::with_capacity(total);
        for (i, &m) in low.iter().enumerate() {
            let t = i as f32 / sr as f32;
            let high = 0.08 * (TAU * 19_500.0 * t).sin() + 0.06 * (TAU * 20_200.0 * t).sin();
            left.push((m + high).clamp(-0.95, 0.95));
            right.push((m - high).clamp(-0.95, 0.95));
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("antiphase_stereo.wav");
        let data_bytes = (total * 4) as u32;
        let riff_size = 36 + data_bytes;
        let mut wav = Vec::with_capacity(44 + data_bytes as usize);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&riff_size.to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(b"fmt ");
        wav.extend_from_slice(&16u32.to_le_bytes());
        wav.extend_from_slice(&1u16.to_le_bytes());
        wav.extend_from_slice(&2u16.to_le_bytes()); // stereo
        wav.extend_from_slice(&sr.to_le_bytes());
        wav.extend_from_slice(&(sr * 4).to_le_bytes());
        wav.extend_from_slice(&4u16.to_le_bytes());
        wav.extend_from_slice(&16u16.to_le_bytes());
        wav.extend_from_slice(b"data");
        wav.extend_from_slice(&data_bytes.to_le_bytes());
        for (&l, &r) in left.iter().zip(&right) {
            let ql = (l.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            let qr = (r.clamp(-1.0, 1.0) * i16::MAX as f32).round() as i16;
            wav.extend_from_slice(&ql.to_le_bytes());
            wav.extend_from_slice(&qr.to_le_bytes());
        }
        std::fs::write(&path, wav).unwrap();

        assert!(verify_downloaded_audio(&path, VerifyPolicy::ArchivalLossless, None).is_ok());
    }
}
