// Enhance with previous search
// https://docs.beets.io/en/latest/reference/pathformat.html#available-values
// beet modify mb_trackid=<track-mbid> track_path
// beet modify mb_albumid=<album-mbid> track_path
// beet modify mb_artistid=<artist-mbid> track_path

// Analysis
// https://beets.readthedocs.io/en/v1.4.6/plugins/acousticbrainz.html

// beets import -m (-s?) /download_folder/album
// beets should be configured to ignore missing album tracks

// Example config

// import:
//   copy: no
//   move: yes
//   resume: no
//   duplicate_action: remove
// paths:
//   default: $albumartist/$album%aunique{}/$track $title
//   singleton: $albumartist/$album%aunique{}/$title
// match:
//   strong_rec_thresh: 0.10
//   max_rec:
//     missing_tracks: low
// musicbrainz:
//   searchlimit: 20            # Recommendation from: https://github.com/kernitus/beets-oldestdate
//   extra_tags:                # Enable improved MediaBrainz queries from tags.
//     [
//       catalognum,
//       country,
//       label,
//       media,
//       year
//     ]
