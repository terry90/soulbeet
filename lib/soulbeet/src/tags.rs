//! Tags a backend writes into a file it fetched untagged, so the importer sees what was asked
//! for.

use lofty::config::WriteOptions;
use lofty::file::TaggedFileExt;
use lofty::tag::{ItemKey, Tag, TagExt};
use std::path::Path;

/// The tags of the track a file was fetched for, from the metadata the search was for.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct WantedTags {
    pub title: String,
    pub artist: String,
    pub album: String,
    pub album_artist: String,
    pub track_no: Option<u32>,
    pub track_total: Option<u32>,
    pub date: Option<String>,
    /// Set only when the file is a `close_match` of the recording: the importer trusts it.
    pub recording_mbid: Option<String>,
    /// Set only when the file is a `close_match` of the release.
    pub release_mbid: Option<String>,
    pub comment: Option<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum TagError {
    #[error("could not read the tags of {path}: {source}")]
    Read {
        path: String,
        source: lofty::error::FileParseError,
    },
    #[error("could not write the tags of {path}: {source}")]
    Write {
        path: String,
        source: lofty::error::FileEncodingError,
    },
}

/// Replaces every tag of the file at `path` with `tags`: Vorbis comments for FLAC and Ogg Opus,
/// ID3v2.4 for MP3.
pub fn write_tags(path: &Path, tags: &WantedTags) -> Result<(), TagError> {
    let shown = || path.display().to_string();
    let read = |source| TagError::Read {
        path: shown(),
        source,
    };
    let write = |source| TagError::Write {
        path: shown(),
        source,
    };
    let file = lofty::read_from_path(path).map_err(read)?;
    let options = WriteOptions::default();
    for old in file.tags() {
        old.tag_type()
            .remove_from_path(path, options)
            .map_err(write)?;
    }
    let mut tag = Tag::new(file.primary_tag_type());
    let texts = [
        (ItemKey::TrackTitle, Some(tags.title.clone())),
        (ItemKey::TrackArtist, Some(tags.artist.clone())),
        (ItemKey::AlbumTitle, Some(tags.album.clone())),
        (ItemKey::AlbumArtist, Some(tags.album_artist.clone())),
        (ItemKey::TrackNumber, tags.track_no.map(|n| n.to_string())),
        (ItemKey::TrackTotal, tags.track_total.map(|n| n.to_string())),
        (ItemKey::RecordingDate, tags.date.clone()),
        (ItemKey::MusicBrainzRecordingId, tags.recording_mbid.clone()),
        (ItemKey::MusicBrainzReleaseId, tags.release_mbid.clone()),
        (ItemKey::Comment, tags.comment.clone()),
    ];
    for (key, text) in texts {
        if let Some(text) = text.filter(|t| !t.is_empty()) {
            tag.insert_text(key, text);
        }
    }
    tag.save_to_path(path, options).map_err(write)
}

/// Whether a file matched its wanted track closely enough to carry the MusicBrainz ids: title
/// and album scores of 0.8 or more, and a known length within 3 s.
pub fn close_match(title_score: f64, album_score: f64, length_diff_s: Option<u32>) -> bool {
    title_score >= 0.8 && album_score >= 0.8 && length_diff_s.is_some_and(|d| d <= 3)
}

#[cfg(test)]
mod tests {
    use super::*;
    use lofty::file::{AudioFile, FileType};
    use lofty::tag::TagType;

    /// CRC-8 (poly 0x07) of a FLAC frame header.
    fn crc8(bytes: &[u8]) -> u8 {
        let mut crc = 0u8;
        for &b in bytes {
            crc ^= b;
            for _ in 0..8 {
                crc = if crc & 0x80 != 0 {
                    (crc << 1) ^ 0x07
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// CRC-16 (poly 0x8005) of a FLAC frame.
    fn crc16(bytes: &[u8]) -> u16 {
        let mut crc = 0u16;
        for &b in bytes {
            crc ^= u16::from(b) << 8;
            for _ in 0..8 {
                crc = if crc & 0x8000 != 0 {
                    (crc << 1) ^ 0x8005
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// A mono 16-bit 44.1 kHz FLAC of `frames` frames of 4096 silent samples.
    fn flac_of_silence(frames: u64) -> Vec<u8> {
        let samples = frames * 4096;
        let mut out = b"fLaC".to_vec();
        // The last metadata block, STREAMINFO, 34 bytes.
        out.extend([0x80, 0, 0, 34]);
        out.extend(4096u16.to_be_bytes());
        out.extend(4096u16.to_be_bytes());
        out.extend([0; 6]);
        let packed: u64 = (44_100u64 << 44) | (15 << 36) | samples;
        out.extend(packed.to_be_bytes());
        out.extend([0; 16]);
        for n in 0..frames {
            // Fixed blocks of 4096 samples at 44.1 kHz, mono, 16 bits, frame number `n` < 128.
            let mut frame = vec![0xFF, 0xF8, 0xC9, 0x08, n as u8];
            frame.push(crc8(&frame));
            // A CONSTANT subframe of value 0.
            frame.extend([0x00, 0x00, 0x00]);
            frame.extend(crc16(&frame).to_be_bytes());
            out.extend(frame);
        }
        out
    }

    /// `frames` silent MPEG-1 Layer III frames, 128 kbit/s at 44.1 kHz.
    fn mp3_of_silence(frames: usize) -> Vec<u8> {
        let mut out = Vec::new();
        for _ in 0..frames {
            let mut frame = vec![0u8; 417];
            frame[..4].copy_from_slice(&[0xFF, 0xFB, 0x90, 0x64]);
            out.extend(frame);
        }
        out
    }

    /// The CRC of an Ogg page (poly 0x04c11db7, not reflected).
    fn ogg_crc(bytes: &[u8]) -> u32 {
        let mut crc = 0u32;
        for &b in bytes {
            crc ^= u32::from(b) << 24;
            for _ in 0..8 {
                crc = if crc & 0x8000_0000 != 0 {
                    (crc << 1) ^ 0x04c1_1db7
                } else {
                    crc << 1
                };
            }
        }
        crc
    }

    /// One Ogg page of stream 1 holding `packets`, each under 255 bytes.
    fn ogg_page(kind: u8, granule: u64, sequence: u32, packets: &[&[u8]]) -> Vec<u8> {
        let mut page = b"OggS".to_vec();
        page.extend([0, kind]);
        page.extend(granule.to_le_bytes());
        page.extend(1u32.to_le_bytes());
        page.extend(sequence.to_le_bytes());
        page.extend([0; 4]);
        page.push(packets.len() as u8);
        for packet in packets {
            page.push(packet.len() as u8);
        }
        for packet in packets {
            page.extend(*packet);
        }
        let crc = ogg_crc(&page);
        page[22..26].copy_from_slice(&crc.to_le_bytes());
        page
    }

    /// A mono Ogg Opus of `packets` silent 20 ms packets.
    fn opus_of_silence(packets: usize) -> Vec<u8> {
        let mut head = b"OpusHead".to_vec();
        head.push(1);
        head.push(1);
        head.extend(312u16.to_le_bytes());
        head.extend(48_000u32.to_le_bytes());
        head.extend([0, 0, 0]);
        let mut tags = b"OpusTags".to_vec();
        tags.extend(4u32.to_le_bytes());
        tags.extend(b"test");
        tags.extend(0u32.to_le_bytes());
        // A CELT fullband 20 ms frame with no data: silence.
        let audio: Vec<&[u8]> = vec![&[0xF8]; packets];
        let granule = 312 + 960 * packets as u64;
        let mut out = ogg_page(0x02, 0, 0, &[&head]);
        out.extend(ogg_page(0x00, 0, 1, &[&tags]));
        out.extend(ogg_page(0x04, granule, 2, &audio));
        out
    }

    fn wanted() -> WantedTags {
        WantedTags {
            title: "Wake Up".into(),
            artist: "Kowloon".into(),
            album: "Come Over".into(),
            album_artist: "Kowloon".into(),
            track_no: Some(7),
            track_total: Some(11),
            date: Some("2026-03-13".into()),
            recording_mbid: Some("5c0e0f5c-8c35-4b1d-9d0a-1f6f2a8b9c01".into()),
            release_mbid: Some("2b4e4a77-0e57-4d3e-8b4f-5d2f7c9e3a10".into()),
            comment: Some("Lossy: test".into()),
        }
    }

    /// The tags read back: the file's only tag, of its primary type.
    fn read_back(path: &Path) -> (FileType, Tag) {
        let file = lofty::read_from_path(path).unwrap();
        let types: Vec<TagType> = file.tags().iter().map(|t| t.tag_type()).collect();
        assert_eq!(types.len(), 1, "{types:?}");
        let tag = file.primary_tag().expect("a primary tag").clone();
        (file.file_type(), tag)
    }

    fn text(tag: &Tag, key: ItemKey) -> Option<&str> {
        tag.get_string(key)
    }

    #[test]
    fn tags_are_written_and_read_back_on_flac_mp3_and_opus() {
        let dir = tempfile::tempdir().unwrap();
        for (name, bytes, kind, tag_type) in [
            (
                "a.flac",
                flac_of_silence(3),
                FileType::Flac,
                TagType::VorbisComments,
            ),
            ("a.mp3", mp3_of_silence(40), FileType::Mpeg, TagType::Id3v2),
            (
                "a.opus",
                opus_of_silence(50),
                FileType::Opus,
                TagType::VorbisComments,
            ),
        ] {
            let path = dir.path().join(name);
            std::fs::write(&path, bytes).unwrap();
            // Tags the fetched file came with, which the importer must not see.
            let mut old = Tag::new(tag_type);
            old.insert_text(ItemKey::TrackTitle, "Not this".into());
            old.insert_text(ItemKey::EncoderSoftware, "Lavf56.4.101".into());
            old.save_to_path(&path, WriteOptions::default()).unwrap();

            write_tags(&path, &wanted()).unwrap();

            let (file_type, tag) = read_back(&path);
            assert_eq!((file_type, tag.tag_type()), (kind, tag_type), "{name}");
            for (key, value) in [
                (ItemKey::TrackTitle, "Wake Up"),
                (ItemKey::TrackArtist, "Kowloon"),
                (ItemKey::AlbumTitle, "Come Over"),
                (ItemKey::AlbumArtist, "Kowloon"),
                (ItemKey::TrackNumber, "7"),
                (ItemKey::TrackTotal, "11"),
                (ItemKey::RecordingDate, "2026-03-13"),
                (
                    ItemKey::MusicBrainzRecordingId,
                    "5c0e0f5c-8c35-4b1d-9d0a-1f6f2a8b9c01",
                ),
                (
                    ItemKey::MusicBrainzReleaseId,
                    "2b4e4a77-0e57-4d3e-8b4f-5d2f7c9e3a10",
                ),
                (ItemKey::Comment, "Lossy: test"),
            ] {
                assert_eq!(text(&tag, key), Some(value), "{name} {key:?}");
            }
            // An Ogg file keeps its vendor string, which reads as the encoder.
            assert_ne!(
                text(&tag, ItemKey::EncoderSoftware),
                Some("Lavf56.4.101"),
                "{name}"
            );
            let file = lofty::read_from_path(&path).unwrap();
            assert!(!file.properties().duration().is_zero(), "{name}");
        }
    }

    #[test]
    fn musicbrainz_ids_go_only_on_a_close_match() {
        for (title, album, diff, close) in [
            (0.8, 0.8, Some(3), true),
            (1.0, 0.95, Some(0), true),
            (0.79, 1.0, Some(0), false),
            (1.0, 0.79, Some(0), false),
            (1.0, 1.0, Some(4), false),
            (1.0, 1.0, None, false),
        ] {
            assert_eq!(
                close_match(title, album, diff),
                close,
                "{title} {album} {diff:?}"
            );
        }

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        std::fs::write(&path, flac_of_silence(1)).unwrap();
        let loose = WantedTags {
            recording_mbid: None,
            release_mbid: None,
            ..wanted()
        };
        write_tags(&path, &loose).unwrap();
        let (_, tag) = read_back(&path);
        assert_eq!(text(&tag, ItemKey::TrackTitle), Some("Wake Up"));
        assert_eq!(text(&tag, ItemKey::MusicBrainzRecordingId), None);
        assert_eq!(text(&tag, ItemKey::MusicBrainzReleaseId), None);
    }

    #[test]
    fn a_file_that_is_no_audio_fails_to_read() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.flac");
        std::fs::write(&path, b"<html>not found</html>").unwrap();
        let e = write_tags(&path, &wanted()).unwrap_err();
        assert!(matches!(e, TagError::Read { .. }), "{e}");
    }
}
