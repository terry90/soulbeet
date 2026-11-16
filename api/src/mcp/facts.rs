//! The fact documents tool results carry: a candidate source (design 11.4) and what a search
//! read (design 10.2). Text from peers and providers is cut to TEXT_MAX_CHARS.

use serde_json::{json, Map, Value};
use shared::download::{DownloadQuery, DownloadableItem, SearchStats};
use shared::failure::{cut_chars, TEXT_MAX_CHARS};
use shared::recommendation::normalize_for_matching;

use crate::core::sources::{Candidate, DroppedByFilter};
use crate::ledger::StoredSpec;
use crate::services::DEFAULT_DOWNLOAD_BACKEND;

/// Peer or provider text as a result shows it.
fn cut(text: &str) -> String {
    cut_chars(text.to_string(), TEXT_MAX_CHARS)
}

/// The wanted length of the file `item` stands for: the wanted track of its title, or the one
/// wanted track of a track search.
fn wanted_duration(item: &DownloadableItem, wanted: &DownloadQuery) -> Option<u32> {
    let title = normalize_for_matching(&item.title);
    match wanted.tracks.as_slice() {
        [only] if wanted.album.is_none() => only.duration_secs(),
        tracks => tracks
            .iter()
            .find(|t| normalize_for_matching(&t.title) == title)
            .and_then(|t| t.duration_secs()),
    }
}

/// One file's own facts.
fn file_fields(item: &DownloadableItem, wanted: &DownloadQuery) -> Map<String, Value> {
    let f = &item.facts;
    let delta = item
        .duration
        .zip(wanted_duration(item, wanted))
        .map(|(file, wanted)| file.abs_diff(wanted));
    let Value::Object(fields) = json!({
        "remote_file": cut(&item.id),
        "extension": f.extension,
        "size_bytes": item.size,
        "bitrate_kbps": f.bitrate_kbps,
        "variable_bitrate": f.variable_bitrate,
        "sample_rate_hz": f.sample_rate_hz,
        "bit_depth": f.bit_depth,
        "duration_s": item.duration,
        "duration_delta_s": delta,
        "match_score": item.match_score,
        "quality_score": item.quality_score,
        "below_floor": item.below_floor,
        "backend_facts": f.backend_facts,
    }) else {
        unreachable!("an object literal is an object");
    };
    fields
}

/// The folder holding every one of `ids`, each a remote path with `\` or `/` between its
/// parts: their longest common parent.
fn common_parent<'a>(mut ids: impl Iterator<Item = &'a str>) -> String {
    let separators = ['\\', '/'];
    let parent = |id: &'a str| &id[..id.rfind(separators).unwrap_or(0)];
    let Some(first) = ids.next() else {
        return String::new();
    };
    let mut common = parent(first);
    for id in ids {
        let folder = parent(id);
        while !(common.is_empty()
            || folder == common
            || folder
                .strip_prefix(common)
                .is_some_and(|rest| rest.starts_with(separators)))
        {
            common = &common[..common.rfind(separators).unwrap_or(0)];
        }
    }
    common.to_string()
}

/// A candidate as tools show it (design 11.4), its durations against the hydrated `wanted`
/// query. A track search's one-file group is offered as that file; any other group as its
/// folder with each file, and for an album the wanted tracks found in it and the titles of
/// those missing.
pub(crate) fn candidate_document(candidate: &Candidate, wanted: &DownloadQuery) -> Value {
    let group = &candidate.group;
    let mut doc = Map::new();
    doc.insert("candidate_id".into(), json!(candidate.id));
    doc.insert("backend".into(), json!(DEFAULT_DOWNLOAD_BACKEND));
    doc.insert("source".into(), json!(cut(&group.source)));
    let first = group.items.first();
    match group.items.as_slice() {
        [item] if wanted.album.is_none() => {
            doc.insert("download_scope".into(), json!("file"));
            doc.insert("artist".into(), json!(cut(&item.artist)));
            doc.insert("album".into(), json!(cut(&item.album)));
            doc.extend(file_fields(item, wanted));
        }
        items => {
            doc.insert("download_scope".into(), json!("folder"));
            doc.insert(
                "remote_folder".into(),
                json!(cut(&common_parent(items.iter().map(|i| i.id.as_str())))),
            );
            doc.insert("artist".into(), json!(group.artist.as_deref().map(cut)));
            doc.insert("album".into(), json!(cut(&group.title)));
            doc.insert("size_bytes".into(), json!(group.total_size));
            doc.insert(
                "below_floor".into(),
                json!(items.iter().any(|i| i.below_floor)),
            );
            let files: Vec<Value> = items
                .iter()
                .map(|item| Value::Object(file_fields(item, wanted)))
                .collect();
            doc.insert("files".into(), Value::Array(files));
            if wanted.album.is_some() {
                let found: Vec<String> = items
                    .iter()
                    .map(|i| normalize_for_matching(&i.title))
                    .collect();
                let (present, missing): (Vec<_>, Vec<_>) = wanted
                    .tracks
                    .iter()
                    .partition(|t| found.contains(&normalize_for_matching(&t.title)));
                doc.insert("tracks_found".into(), json!(present.len()));
                doc.insert(
                    "missing_titles".into(),
                    json!(missing.iter().map(|t| cut(&t.title)).collect::<Vec<_>>()),
                );
            }
        }
    }
    let facts = first.map(|item| &item.facts);
    doc.insert("score".into(), json!(group.score));
    doc.insert(
        "folder_track_count".into(),
        json!(facts.and_then(|f| f.folder_audio_files)),
    );
    doc.insert(
        "peer_has_free_slot".into(),
        json!(facts.and_then(|f| f.source_has_free_slot)),
    );
    doc.insert(
        "peer_upload_speed".into(),
        json!(facts.and_then(|f| f.source_upload_speed)),
    );
    doc.insert(
        "peer_queue_length".into(),
        json!(facts.and_then(|f| f.source_queue_length)),
    );
    doc.insert(
        "peer_recent_failures".into(),
        json!(candidate.recent_failures),
    );
    doc.insert("peer_flagged".into(), json!(candidate.flagged()));
    Value::Object(doc)
}

/// What a search read and what was dropped (design 10.2), each null when the backend did not
/// finish the search (`stats` None). Files scored below the floor are dropped unless the
/// search kept them, flagged, as candidates.
pub(crate) fn search_facts(
    stats: Option<&SearchStats>,
    spec: &StoredSpec,
    dropped: &DroppedByFilter,
) -> Map<String, Value> {
    let Value::Object(facts) = (match stats {
        Some(s) => json!({
            "query_text_sent": cut(&s.query_text),
            "response_count": s.response_count,
            "file_count": s.file_count,
            "locked_file_count": s.locked_file_count,
            "dropped_below_floor": if spec.keep_below_floor { 0 } else { s.below_floor },
            "dropped_by_filter": dropped,
        }),
        None => json!({
            "query_text_sent": null,
            "response_count": null,
            "file_count": null,
            "locked_file_count": null,
            "dropped_below_floor": null,
            "dropped_by_filter": null,
        }),
    }) else {
        unreachable!("an object literal is an object");
    };
    facts
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::server_fns::download::test_support::{group, item, track};
    use shared::download::FileFacts;

    #[test]
    fn a_folder_is_the_common_parent_of_its_files() {
        assert_eq!(
            common_parent(["a\\Come Over\\CD1\\07.flac", "a\\Come Over\\CD2\\01.flac"].into_iter()),
            "a\\Come Over"
        );
        assert_eq!(
            common_parent(["music/Kowloon/07.flac", "music/Kowloon/08.flac"].into_iter()),
            "music/Kowloon"
        );
        // A folder whose name starts like another's is not its parent.
        assert_eq!(
            common_parent(["a\\Come\\07.flac", "a\\Come Over\\08.flac"].into_iter()),
            "a"
        );
        assert_eq!(common_parent(["07.flac"].into_iter()), "");
        assert_eq!(common_parent(["07.flac", "x\\08.flac"].into_iter()), "");
    }

    #[test]
    fn a_candidate_of_a_track_search_is_its_file_with_its_peer_and_failures() {
        let mut wanted = DownloadQuery::new(vec![track("Kowloon", "Wake Up")]);
        wanted.tracks[0].duration = Some("3:30".into());
        let file = DownloadableItem {
            duration: Some(205),
            facts: FileFacts {
                extension: Some("flac".into()),
                folder_audio_files: Some(11),
                source_queue_length: Some(4),
                ..FileFacts::default()
            },
            ..item("peer_a", "Wake Up", &format!("a\\{}.flac", "x".repeat(600)))
        };
        let candidate = Candidate {
            id: "s1:2".into(),
            group: group("peer_a", vec![file], 0.8),
            recent_failures: 3,
        };
        let doc = candidate_document(&candidate, &wanted);
        assert_eq!(
            (
                doc["download_scope"].as_str(),
                doc["backend"].as_str(),
                doc["duration_delta_s"].as_u64(),
                doc["folder_track_count"].as_u64(),
                doc["peer_queue_length"].as_u64()
            ),
            (Some("file"), Some("slskd"), Some(5), Some(11), Some(4))
        );
        assert_eq!(
            (
                doc["peer_recent_failures"].as_u64(),
                doc["peer_flagged"].as_bool()
            ),
            (Some(3), Some(true))
        );
        assert!(doc["remote_file"].as_str().unwrap().chars().count() <= TEXT_MAX_CHARS);
        assert!(doc.get("files").is_none() && doc.get("tracks_found").is_none());
    }

    #[test]
    fn a_search_that_did_not_finish_reports_no_counts() {
        let facts = search_facts(None, &StoredSpec::default(), &DroppedByFilter::default());
        assert!(facts.values().all(Value::is_null), "{facts:?}");
        let stats = SearchStats {
            query_text: "Kowloon Wake Up".into(),
            below_floor: 7,
            ..SearchStats::default()
        };
        let kept = StoredSpec {
            keep_below_floor: true,
            ..StoredSpec::default()
        };
        let dropped = DroppedByFilter {
            format: 2,
            ..DroppedByFilter::default()
        };
        assert_eq!(
            Value::Object(search_facts(Some(&stats), &kept, &dropped)),
            json!({"query_text_sent": "Kowloon Wake Up", "response_count": 0, "file_count": 0,
                "locked_file_count": 0, "dropped_below_floor": 0,
                "dropped_by_filter": {"format": 2, "bitrate": 0, "not_lossless": 0,
                    "queue_length": 0, "excluded": 0}})
        );
        assert_eq!(
            search_facts(Some(&stats), &StoredSpec::default(), &dropped)["dropped_below_floor"],
            7
        );
    }
}
