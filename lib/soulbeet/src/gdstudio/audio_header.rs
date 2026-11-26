//! What the first bytes of a NetEase file say it is: a FLAC's STREAMINFO, or an MP3's ID3v2 tag
//! and frame sync.

/// How many bytes of a file the search reads to know it.
pub(crate) const HEAD_BYTES: u64 = 4096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Header {
    Flac {
        bits: u32,
        rate: u32,
        channels: u32,
        samples: u64,
    },
    /// `id3` is the size of the ID3v2 tag before the frames, its header included; 0 for none.
    Mp3 { id3: u64 },
}

impl Header {
    pub(crate) fn ext(&self) -> &'static str {
        match self {
            Header::Flac { .. } => "flac",
            Header::Mp3 { .. } => "mp3",
        }
    }

    /// The file's length in seconds: exact for FLAC, from the size and the average bitrate
    /// for MP3, which is exact for a constant bitrate.
    pub(crate) fn seconds(&self, size: u64, kbps: u32) -> Option<f64> {
        match *self {
            Header::Flac { rate, samples, .. } if rate > 0 && samples > 0 => {
                Some(samples as f64 / f64::from(rate))
            }
            Header::Flac { .. } => None,
            Header::Mp3 { id3 } if kbps > 0 && size > id3 => {
                Some((size - id3) as f64 * 8.0 / (f64::from(kbps) * 1000.0))
            }
            Header::Mp3 { .. } => None,
        }
    }
}

/// The header `bytes`, the start of a file, begin with.
pub(crate) fn read(bytes: &[u8]) -> Result<Header, String> {
    if bytes.starts_with(b"fLaC") {
        return flac(bytes);
    }
    if bytes.starts_with(b"ID3") {
        return id3(bytes);
    }
    if frame_sync(bytes) {
        return Ok(Header::Mp3 { id3: 0 });
    }
    let start: String = bytes.iter().take(8).map(|b| format!("{b:02x}")).collect();
    Err(format!(
        "the file starts with {start}, neither FLAC nor MP3"
    ))
}

fn frame_sync(bytes: &[u8]) -> bool {
    matches!(bytes, [0xFF, second, ..] if second & 0xE0 == 0xE0)
}

fn flac(bytes: &[u8]) -> Result<Header, String> {
    // The first metadata block is STREAMINFO, 34 bytes, after the marker and its own header.
    let Some(info) = bytes.get(8..42) else {
        return Err("the FLAC header ends before its STREAMINFO".into());
    };
    if bytes[4] & 0x7F != 0 || bytes[5..8] != [0, 0, 34] {
        return Err("the FLAC header does not begin with a STREAMINFO block".into());
    }
    let rate =
        (u32::from(info[10]) << 12) | (u32::from(info[11]) << 4) | (u32::from(info[12]) >> 4);
    let channels = u32::from((info[12] >> 1) & 0x07) + 1;
    let bits = ((u32::from(info[12] & 0x01) << 4) | (u32::from(info[13]) >> 4)) + 1;
    let samples = (u64::from(info[13] & 0x0F) << 32)
        | u64::from(u32::from_be_bytes([info[14], info[15], info[16], info[17]]));
    if rate == 0 {
        return Err("the FLAC STREAMINFO gives no sample rate".into());
    }
    Ok(Header::Flac {
        bits,
        rate,
        channels,
        samples,
    })
}

fn id3(bytes: &[u8]) -> Result<Header, String> {
    let Some(head) = bytes.get(..10) else {
        return Err("the ID3 tag ends before its header".into());
    };
    if head[6..10].iter().any(|b| b & 0x80 != 0) {
        return Err("the ID3 tag size is not synchsafe".into());
    }
    let size = head[6..10]
        .iter()
        .fold(0u64, |size, b| (size << 7) | u64::from(*b));
    let footer = if head[5] & 0x10 != 0 { 10 } else { 0 };
    let id3 = 10 + size + footer;
    // A tag longer than the bytes read hides the first frame; the tag alone says MP3.
    match usize::try_from(id3).ok().and_then(|at| bytes.get(at..)) {
        Some(rest) if rest.len() >= 2 && !frame_sync(rest) => {
            Err("no MP3 frame follows the ID3 tag".into())
        }
        _ => Ok(Header::Mp3 { id3 }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/src/gdstudio/fixtures/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn a_flac_header_gives_bit_depth_rate_and_exact_length() {
        for (name, bits, rate, seconds) in [
            ("r1_flac.meta", 16, 44_100, 320.357),
            ("u6_pomme1_br999.meta", 24, 48_000, 0.0),
            ("u10_okc1.meta", 24, 44_100, 287.95),
            // Its one comment is the encoder, so it is as untagged as the others.
            ("u11_okc3.meta", 16, 44_100, 267.693),
        ] {
            let header = read(&fixture(name)).unwrap();
            let Header::Flac {
                bits: b,
                rate: r,
                channels,
                samples,
            } = header
            else {
                panic!("{name}: {header:?}");
            };
            assert_eq!((b, r, channels), (bits, rate, 2), "{name}");
            assert!(samples > 0, "{name}");
            if seconds > 0.0 {
                let read = header.seconds(0, 0).unwrap();
                assert!((read - seconds).abs() < 0.01, "{name}: {read}");
            }
            assert_eq!(header.ext(), "flac");
        }
    }

    #[test]
    fn an_mp3_header_gives_its_tag_size_and_a_length_from_size_and_bitrate() {
        for name in ["r2_mp3.meta", "u9_midori1_br999.meta"] {
            assert_eq!(
                read(&fixture(name)).unwrap(),
                Header::Mp3 { id3: 45 },
                "{name}"
            );
        }
        // u4: 12816762 bytes at 320 kbps.
        let seconds = Header::Mp3 { id3: 45 }.seconds(12_816_762, 320).unwrap();
        assert!((seconds - 320.42).abs() < 0.01, "{seconds}");
        let mut framed = vec![0xFF, 0xFB, 0x90, 0x64];
        framed.resize(417, 0);
        assert_eq!(read(&framed).unwrap(), Header::Mp3 { id3: 0 });
        let mut tagged = fixture("r2_mp3.meta");
        tagged.extend(&framed);
        assert_eq!(read(&tagged).unwrap(), Header::Mp3 { id3: 45 });
    }

    #[test]
    fn anything_else_is_neither() {
        for bytes in [
            &b"<html><head><title>403 Forbidden"[..],
            &b"fLaC\x84\x00\x00\x22"[..],
            &b"ID3\x04\x00\x00\x00\x00\x00\x00ab"[..],
            &b""[..],
        ] {
            assert!(read(bytes).is_err(), "{bytes:?}");
        }
    }
}
