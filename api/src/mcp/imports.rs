//! The import decision tools (design 11.6): `get_import_choices` shows what a job waiting for an
//! import decision kept, `resolve_import` decides it.

use rmcp::model::{CallToolResult, JsonObject};
use serde_json::{json, Map, Value};
use shared::download::MatchCandidate;
use shared::failure::{cut_chars, RetryAdvice, RAW_MAX_CHARS, TEXT_MAX_CHARS};

use super::args::Args;
use super::contract::{
    answer, failed, invalid_value, success, upstream_error, NextCall, ToolError,
};
use super::jobs::JobTool;
use super::keys::{keyed, request_key, Keyed};
use super::paging::fit_whole;
use super::refs::request_key_schema;
use super::server::{common_codes, ToolSpec};
use super::McpCaller;
use crate::connection::ConnectionState;
use crate::core::jobs::decide::{import_choices, resolve, Decision, ImportChoices};

const GET_IMPORT_CHOICES_ARGS: &[&str] = &["job_id", "slot"];

const RESOLVE_IMPORT_ARGS: &[&str] = &[
    "job_id",
    "slot",
    "choice",
    "data_source",
    "id",
    "musicbrainz",
    "request_key",
];

/// resolve_import's choices, as the schema lists them.
const CHOICES: [&str; 5] = ["retry", "as_is", "candidate", "musicbrainz", "discard"];

pub(crate) const GET_IMPORT_CHOICES: ToolSpec = ToolSpec {
    name: "get_import_choices",
    title: "Show import choices",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Shows what a job waiting for an import decision (needs_decision) kept for it: kept_folder, the folder of the downloaded files relative to Soulful's download folder; files, each file relative to kept_folder with the tags beets read (artist, title, album, track, length_s), files_omitted counting those left out to stay under 60,000 characters; tasks, how many of beets' tasks imported of how many, and output_tail, the end of beets' output, from its last run; candidates, the matches beets weighed and found too far (data_source, id, artist, title, album, year, country, disambiguation, distance from 0 for an exact match to 1, the fields that differ, and for an album how the files fit its tracks); and allowed_choices, the choices resolve_import takes for it. A job of an album the web page imported track by track can keep several imports: slot then names one, any slot waiting for it. Failure codes: {common}, unknown_handle (no job of this user has this id, or another decision already took the kept import), wrong_job_state (the job is not waiting for an import decision), import_files_gone (the kept files were deleted; resolve_import or dismiss_job ends the job).")
    },
    input_schema: r#"{"type": "object", "additionalProperties": false, "required": ["job_id"], "properties": {"job_id": {"type": "string", "minLength": 1}, "slot": {"type": "integer", "minimum": 0}}}"#,
    read_only: true,
    destructive: false,
    idempotent: true,
    open_world: false,
    call: |state, caller, args| Box::pin(get_import_choices(state, caller, args)),
};

pub(crate) const RESOLVE_IMPORT: ToolSpec = ToolSpec {
    name: "resolve_import",
    title: "Decide an import",
    description: |calls_a_minute| {
        let common = common_codes(calls_a_minute);
        format!("Decides the import a job waiting for an import decision (needs_decision) kept, with choice: retry (beets matches again on its own), as_is (the files are imported with their own tags), candidate (one of get_import_choices' candidates, by its data_source and id), musicbrainz (the MusicBrainz recording, for a track job, or release, for an album job, given as an MBID or a musicbrainz.org link), or discard (deletes the downloaded copy Soulful kept, never a library file, and dismisses the job's slots waiting for it). musicbrainz and as_is work without candidates, so a recording found with search_music can always be applied. slot as get_import_choices takes it. Answers the job's full document, as get_jobs gives it; the import then runs, so follow it with get_jobs. Failure codes: {common}, unknown_handle (no job of this user has this id, or another decision already took the kept import), wrong_job_state (the job is not waiting for an import decision), import_files_gone (the kept files were deleted; the job's slots fail), import_choice_invalid (a candidate beets did not find, a MusicBrainz link to the wrong entity), already_running (a call with this request_key still runs).")
    },
    input_schema: concat!(
        r#"{"type": "object", "additionalProperties": false, "required": ["job_id", "choice"], "properties": {"job_id": {"type": "string", "minLength": 1}, "slot": {"type": "integer", "minimum": 0}, "choice": {"enum": ["retry", "as_is", "candidate", "musicbrainz", "discard"]}, "data_source": {"type": "string", "minLength": 1}, "id": {"type": "string", "minLength": 1}, "musicbrainz": {"type": "string", "minLength": 1}, "#,
        request_key_schema!(),
        "}}"
    ),
    read_only: false,
    destructive: false,
    idempotent: false,
    open_world: false,
    call: |state, caller, args| Box::pin(resolve_import(state, caller, args)),
};

/// `e`, offering `again` (this call) when its failure says the same call can answer later and it
/// offers nothing else, as an upstream error does.
fn offered(e: ToolError, again: Option<NextCall>) -> ToolError {
    match e.next.is_empty() && e.failure.retry == RetryAdvice::SameSourceLater {
        true => upstream_error(e.failure, e.facts, again),
        false => e,
    }
}

/// `get_import_choices`: what the caller's job waiting for an import decision kept.
pub(crate) async fn get_import_choices(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("get_import_choices", args, GET_IMPORT_CHOICES_ARGS);
    answer(
        async {
            let job_id = args.string("job_id")?;
            let slot = args.opt_u32("slot", 0, 999)?;
            args.finish()?;
            let choices = import_choices(&state.acquire, &caller.ctx, &job_id, slot)
                .await
                .map_err(|e| offered(e, args.same_call()))?;
            Ok(choices_document(choices))
        }
        .await,
    )
}

/// Text from beets or a peer as a result shows it.
fn cut(text: &str) -> String {
    cut_chars(text.to_string(), TEXT_MAX_CHARS)
}

/// What get_import_choices answers: the files last, as many as fit.
fn choices_document(choices: ImportChoices) -> Map<String, Value> {
    let candidates: Vec<Value> = (choices.candidates.iter())
        .flat_map(|c| &c.candidates)
        .map(candidate_document)
        .collect();
    let mut fields = Map::new();
    fields.insert("job_id".into(), json!(choices.job.job.id));
    let folder = choices.kept_folder.display().to_string();
    fields.insert("kept_folder".into(), json!(cut(&folder)));
    fields.insert("tasks".into(), json!(choices.tasks));
    let tail = (choices.output_tail).map(|tail| cut_chars(tail, RAW_MAX_CHARS));
    fields.insert("output_tail".into(), json!(tail));
    fields.insert("candidates".into(), Value::Array(candidates));
    fields.insert("allowed_choices".into(), json!(choices.allowed));
    // The tags were cut when beets' report was read.
    let files = (choices.files.iter())
        .map(|f| {
            json!({"file": cut(&f.path.display().to_string()), "artist": f.artist, "title": f.title,
                "album": f.album, "track": f.track, "length_s": f.length_secs})
        })
        .collect();
    fit_whole(fields, "files", files, "files_omitted")
}

/// A match beets weighed: its ids, names, how far it is from the files (0 an exact match), what
/// differs, and how the files fit an album's tracks.
fn candidate_document(c: &MatchCandidate) -> Value {
    json!({
        "data_source": c.data_source,
        "id": c.id,
        "artist": cut(&c.artist),
        "title": cut(&c.title),
        "album": c.album.as_deref().map(cut),
        "year": c.year,
        "country": c.country,
        "disambiguation": c.disambig.as_deref().map(cut),
        "distance": f64::from(1000 - c.per_mille.min(1000)) / 1000.0,
        "differs": c.differs,
        "track_fit": c.tracks,
    })
}

/// A resolve_import call as read.
struct ResolveCall {
    job_id: String,
    slot: Option<u32>,
    /// The choice's name, as CHOICES has it.
    choice: &'static str,
    decision: Decision,
    key: Option<String>,
}

impl ResolveCall {
    /// The call's arguments as read, `request_key` left out: what the key is bound to.
    fn value(&self) -> Value {
        let (data_source, id, musicbrainz) = match &self.decision {
            Decision::Candidate { data_source, id } => (Some(data_source), Some(id), None),
            Decision::MusicBrainz(text) => (None, None, Some(text)),
            Decision::Retry | Decision::AsIs | Decision::Discard => (None, None, None),
        };
        json!({"job_id": self.job_id, "slot": self.slot, "choice": self.choice,
            "data_source": data_source, "id": id, "musicbrainz": musicbrainz})
    }
}

/// resolve_import's arguments: `choice` required, `data_source` and `id` given with candidate
/// only and both, `musicbrainz` with musicbrainz only.
fn read_resolve(args: &Args) -> Result<ResolveCall, ToolError> {
    let job_id = args.string("job_id")?;
    let slot = args.opt_u32("slot", 0, 999)?;
    let refused = || {
        let expected = format!("one of {}", CHOICES.join(", "));
        invalid_value("resolve_import", "choice", &expected)
    };
    let choice = args.opt_choice("choice", &CHOICES)?.ok_or_else(refused)?;
    let decision = match choice {
        "retry" => Decision::Retry,
        "as_is" => Decision::AsIs,
        "candidate" => Decision::Candidate {
            data_source: args.string("data_source")?,
            id: args.string("id")?,
        },
        "musicbrainz" => Decision::MusicBrainz(args.string("musicbrainz")?),
        // discard, the one choice left.
        _ => Decision::Discard,
    };
    if choice != "candidate" {
        args.absent("data_source", "absent unless choice is candidate")?;
        args.absent("id", "absent unless choice is candidate")?;
    }
    if choice != "musicbrainz" {
        args.absent("musicbrainz", "absent unless choice is musicbrainz")?;
    }
    let key = request_key(args)?;
    args.finish()?;
    Ok(ResolveCall {
        job_id,
        slot,
        choice,
        decision,
        key,
    })
}

/// `resolve_import`: the decision on the caller's job's kept import, once per request key. A
/// decision made answers the job's full document; a refusal that changed the job (`changed` in
/// its facts) is kept for the key too, any other lets the key go.
pub(crate) async fn resolve_import(
    state: &ConnectionState,
    caller: &McpCaller,
    args: Option<JsonObject>,
) -> CallToolResult {
    let args = Args::new("resolve_import", args, RESOLVE_IMPORT_ARGS);
    let read = match read_resolve(&args) {
        Ok(read) => read,
        Err(e) => return failed(e),
    };
    let call = read.value();
    let ResolveCall {
        job_id,
        slot,
        decision,
        key,
        ..
    } = read;
    let on = JobTool::of("resolve_import", state, caller);
    let again = args.same_call();
    let work = async move {
        match resolve(&on.acquire, &on.caller.ctx, &job_id, slot, decision).await {
            Ok(view) => Keyed::Kept(match on.document(&view).await {
                Ok(fields) => success(fields),
                Err(f) => failed(upstream_error(f, Map::new(), None)),
            }),
            Err(e) if e.facts.get("changed") == Some(&json!(true)) => Keyed::Kept(failed(e)),
            Err(e) => Keyed::Released(failed(offered(e, again))),
        }
    };
    keyed(&state.acquire, caller, "resolve_import", key, &call, work).await
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::connection::test_util::{app_of, call_tool, document, jobs_app};
    use crate::core::folder_of;
    use crate::core::jobs::test_support::{
        downloading_track_job, kept_track_job_with, no_match_with_files, track_job_of,
    };

    #[tokio::test]
    async fn get_import_choices_needs_a_job_of_the_caller_waiting_for_a_decision() {
        let (db, acq, _, token) = jobs_app().await;
        let app = app_of(&db, &acq);
        let job = downloading_track_job(&acq).await;
        let error = document(
            &call_tool(
                app.clone(),
                &token,
                "get_import_choices",
                json!({"job_id": job}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (
                Some("wrong_job_state"),
                Some(
                    "The job is downloading; get_import_choices needs a job that is needs_decision"
                )
            )
        );
        assert_eq!(error["facts"]["allowed_states"], json!(["needs_decision"]));
        assert_eq!(error["next"][0]["tool"], "get_jobs");
        folder_of(&db, "bob").await;
        let bobs = track_job_of(&acq, "bob").await;
        let error =
            document(&call_tool(app, &token, "get_import_choices", json!({"job_id": bobs})).await)
                ["error"]
                .clone();
        assert_eq!(
            (error["code"].as_str(), error["message"].as_str()),
            (Some("unknown_handle"), Some("Job not found"))
        );
    }

    #[tokio::test]
    async fn a_decision_that_failed_the_job_answers_the_same_for_its_request_key() {
        let (db, acq, effects, token) = jobs_app().await;
        let app = app_of(&db, &acq);
        let job = kept_track_job_with(&acq, &effects, no_match_with_files(1)).await;
        std::fs::remove_dir_all(acq.download_base.join("Come Over [1f2e3d4c]")).unwrap();
        let call = json!({"job_id": job, "choice": "as_is", "request_key": "k1"});
        let first = call_tool(app.clone(), &token, "resolve_import", call.clone()).await;
        let error = &document(&first)["error"];
        assert_eq!(
            (error["code"].as_str(), error["facts"]["state"].as_str()),
            (Some("import_files_gone"), Some("failed"))
        );
        // The slots failed: the repeat answers that, not what the job is now asked again.
        let again = call_tool(app, &token, "resolve_import", call).await;
        assert_eq!(document(&again), document(&first));
    }

    #[tokio::test]
    async fn the_choices_and_a_decision_go_through_the_tools() {
        let (db, acq, effects, token) = jobs_app().await;
        let app = app_of(&db, &acq);
        let job = kept_track_job_with(&acq, &effects, no_match_with_files(2)).await;
        let doc = document(
            &call_tool(
                app.clone(),
                &token,
                "get_import_choices",
                json!({"job_id": job}),
            )
            .await,
        );
        assert_eq!(doc["kept_folder"], "Come Over [1f2e3d4c]", "{doc}");
        assert_eq!(
            doc["files"],
            json!([{"file": "01 - Song Number 1.flac", "artist": "Zzqx Artist",
                "title": "Song Number 1", "album": "Qzz Album", "track": 1, "length_s": 214}])
        );
        assert_eq!(doc["files_omitted"], 0);
        assert_eq!(doc["tasks"], json!({"imported": 0, "total": 1}));
        assert_eq!(doc["output_tail"], "skipped tail");
        let first = &doc["candidates"][0];
        assert_eq!(
            (&first["data_source"], &first["distance"], &first["differs"]),
            (
                &json!("MusicBrainz"),
                &json!(0.388),
                &json!(["album", "track_length"])
            )
        );
        assert_eq!(
            doc["allowed_choices"],
            json!(["retry", "as_is", "candidate", "musicbrainz", "discard"])
        );

        // A pick with no id is refused before anything is decided.
        let error = document(
            &call_tool(
                app.clone(),
                &token,
                "resolve_import",
                json!({"job_id": job, "choice": "candidate", "data_source": "MusicBrainz"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (error["code"].as_str(), error["facts"]["field"].as_str()),
            (Some("invalid_input"), Some("id"))
        );
        let error = document(
            &call_tool(
                app.clone(),
                &token,
                "resolve_import",
                json!({"job_id": job, "choice": "as_is", "musicbrainz": "x"}),
            )
            .await,
        )["error"]
            .clone();
        assert_eq!(
            (
                error["facts"]["field"].as_str(),
                error["facts"]["expected"].as_str()
            ),
            (
                Some("musicbrainz"),
                Some("absent unless choice is musicbrainz")
            )
        );

        let doc = document(
            &call_tool(
                app,
                &token,
                "resolve_import",
                json!({"job_id": job, "choice": "discard", "request_key": "k1"}),
            )
            .await,
        );
        assert_eq!(doc["state"], "dismissed", "{doc}");
        assert!(!acq.download_base.join("Come Over [1f2e3d4c]").exists());
    }
}
