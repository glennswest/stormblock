//! `GET/POST /api/v1/releases` — what this appliance publishes, and how to get it.
//!
//! An image on a shelf is not a release. A release is a version somebody can
//! find, a link they can pull it from, a manifest saying what went into it, and
//! notes saying what changed. All four or none: a download with no manifest is
//! a file of unknown provenance, and a manifest with no download is a promise.
//!
//! The bytes are not copied to publish them. A release names a volume the
//! engine already holds, and the download streams straight out of it — so
//! publishing costs a record, and the image that is served is by construction
//! the image that was built.

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    body::Body,
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
    routing::get,
    Json, Router,
};
use serde::{Deserialize, Serialize};

use super::ApiError;
use crate::mgmt::config::human_size;
use crate::mgmt::AppState;
use crate::volume::VolumeId;

/// How much is read from the volume per chunk while streaming a download.
const CHUNK: u64 = 4 * 1024 * 1024;

/// One thing that went into a release.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManifestEntry {
    /// `binary`, `golden`, `kernel` — whatever the build called it.
    pub kind: String,
    pub name: String,
    /// Content digest. The point of the entry: a name without one says nothing.
    pub digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
    /// Where it came from — a commit, a package version.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provenance: Option<String>,
    /// The component's own version, as it releases itself: `13.4.0`.
    ///
    /// Separate from the digest and from the commit because they answer
    /// different questions. The digest says whether the bytes changed, the
    /// commit says which source produced them, and the version is the only one
    /// a person reads and compares.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// What changed in this component since the last release that carried it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// A published version.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Release {
    /// `10.21`. Unique here, and what the download URL is keyed on.
    pub version: String,
    /// The volume holding the image. Named rather than copied.
    pub volume: String,
    pub volume_id: uuid::Uuid,
    /// What the whole image hashes to, when the publisher recorded it. Absent
    /// is honest; wrong would not be, so it is never computed on a guess.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub digest: Option<String>,
    /// Seconds since the epoch, as the publisher stated it.
    pub created_unix: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
    #[serde(default)]
    pub manifest: Vec<ManifestEntry>,
}

#[derive(Debug, Deserialize)]
pub struct PublishRequest {
    pub version: String,
    /// Volume id or name.
    pub volume: String,
    #[serde(default)]
    pub digest: Option<String>,
    #[serde(default)]
    pub created_unix: Option<u64>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub manifest: Vec<ManifestEntry>,
    /// Move the release this one replaces down to a tier — `cool`, `cold`.
    ///
    /// The policy lives with whoever publishes, because only they know
    /// whether this build supersedes the last one or sits beside it. Absent,
    /// nothing is demoted and the tiers are left as they are.
    #[serde(default)]
    pub demote_previous: Option<String>,
}

fn releases_path(state: &AppState) -> Option<PathBuf> {
    state
        .config
        .management
        .data_dir
        .as_ref()
        .map(|d| PathBuf::from(d).join("releases.json"))
}

async fn load(state: &AppState) -> Vec<Release> {
    let Some(path) = releases_path(state) else { return Vec::new() };
    let Ok(bytes) = std::fs::read(&path) else { return Vec::new() };
    match serde_json::from_slice(&bytes) {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!("failed to parse {}: {e}", path.display());
            Vec::new()
        }
    }
}

async fn save(state: &AppState, releases: &[Release]) -> Result<(), String> {
    let Some(path) = releases_path(state) else {
        return Err("no management.data_dir configured, so a release cannot be recorded".into());
    };
    let bytes = serde_json::to_vec_pretty(releases).map_err(|e| e.to_string())?;
    // Temp file and rename: a crash mid-write must not leave a truncated index
    // behind, because the index is how anyone finds any of this.
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(())
}

/// A date, so the index reads like a release page rather than a log line.
fn civil_date(unix: u64) -> String {
    let days = (unix / 86_400) as i64;
    let secs = unix % 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

#[derive(Debug, Serialize)]
struct ReleaseSummary {
    version: String,
    volume: String,
    size_bytes: u64,
    size_human: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    digest: Option<String>,
    created: String,
    components: usize,
    has_notes: bool,
    image_url: String,
    manifest_url: String,
    notes_url: String,
}

async fn size_of(state: &AppState, r: &Release) -> u64 {
    let vm = state.volume_manager.lock().await;
    vm.get_volume(&VolumeId(r.volume_id))
        .map(|d| d.capacity_bytes())
        .unwrap_or(0)
}

async fn summarise(state: &AppState, r: &Release) -> ReleaseSummary {
    let size = size_of(state, r).await;
    ReleaseSummary {
        version: r.version.clone(),
        volume: r.volume.clone(),
        size_bytes: size,
        size_human: human_size(size),
        digest: r.digest.clone(),
        created: civil_date(r.created_unix),
        components: r.manifest.len(),
        has_notes: r.notes.is_some(),
        image_url: format!("/api/v1/releases/{}/image.img", r.version),
        manifest_url: format!("/api/v1/releases/{}/manifest", r.version),
        notes_url: format!("/api/v1/releases/{}/notes", r.version),
    }
}

/// Newest first, which is the order anyone asking "what is current" wants.
fn sorted(mut releases: Vec<Release>) -> Vec<Release> {
    releases.sort_by(|a, b| b.created_unix.cmp(&a.created_unix));
    releases
}

async fn index(State(state): State<Arc<AppState>>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "releases", "method" => "index").increment(1);
    let releases = sorted(load(&state).await);
    let mut items = Vec::with_capacity(releases.len());
    for r in &releases {
        items.push(summarise(&state, r).await);
    }
    let count = items.len();
    Json(serde_json::json!({ "items": items, "count": count })).into_response()
}

/// The same index for a browser, because a download link nobody can click is
/// not much of a download link.
async fn index_html(State(state): State<Arc<AppState>>) -> Response {
    let releases = sorted(load(&state).await);
    let mut rows = String::new();
    for r in &releases {
        let s = summarise(&state, r).await;
        rows.push_str(&format!(
            "<tr><td><strong>{}</strong></td><td>{}</td><td>{}</td><td>{}</td>\
             <td><a href=\"{}\">image.img</a></td><td><a href=\"{}\">manifest</a></td>\
             <td>{}</td></tr>",
            html_escape(&s.version),
            s.created,
            s.size_human,
            s.components,
            s.image_url,
            s.manifest_url,
            if s.has_notes {
                format!("<a href=\"{}\">notes</a>", s.notes_url)
            } else {
                "—".to_string()
            },
        ));
    }
    if rows.is_empty() {
        rows = "<tr><td colspan=7><em>nothing published yet</em></td></tr>".into();
    }
    Html(format!(
        "<!doctype html><meta charset=utf-8><title>stormcos releases</title>\
         <style>body{{font:14px/1.5 system-ui,sans-serif;margin:2rem;max-width:60rem}}\
         table{{border-collapse:collapse;width:100%}}\
         th,td{{text-align:left;padding:.4rem .8rem;border-bottom:1px solid #ddd}}\
         th{{font-weight:600;border-bottom:2px solid #999}}</style>\
         <h1>stormcos releases</h1>\
         <table><tr><th>version<th>published<th>size<th>components<th>image<th>manifest<th>notes</tr>\
         {rows}</table>"
    ))
    .into_response()
}

fn html_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

async fn get_one(State(state): State<Arc<AppState>>, Path(version): Path<String>) -> Response {
    let releases = load(&state).await;
    match releases.iter().find(|r| r.version == version) {
        Some(r) => {
            let size = size_of(&state, r).await;
            Json(serde_json::json!({
                "release": r,
                "size_bytes": size,
                "size_human": human_size(size),
                "created": civil_date(r.created_unix),
                "image_url": format!("/api/v1/releases/{version}/image.img"),
            }))
            .into_response()
        }
        None => ApiError::not_found(format!("no release {version}")),
    }
}

async fn manifest(State(state): State<Arc<AppState>>, Path(version): Path<String>) -> Response {
    let releases = load(&state).await;
    match releases.iter().find(|r| r.version == version) {
        Some(r) => Json(serde_json::json!({
            "version": r.version,
            "count": r.manifest.len(),
            "items": r.manifest,
        }))
        .into_response(),
        None => ApiError::not_found(format!("no release {version}")),
    }
}

#[derive(Debug, Serialize)]
struct ComponentChange {
    kind: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_version: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    from_provenance: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    to_provenance: Option<String>,
    from_digest: String,
    to_digest: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    notes: Option<String>,
}

/// `GET /api/v1/releases/{version}/changes[?since=X]` — what moved.
///
/// The manifest says what a release is made of; this says how it differs from
/// the one before it, which is the question anyone actually asks of a release.
/// Comparison is by content digest, so a component rebuilt from the same source
/// with the same result is *not* a change — a rebuild is not news, and a list
/// that says otherwise is one nobody reads twice.
///
/// `since` names a specific release to compare against; without it the
/// comparison is against the release published immediately before this one.
async fn changes(
    State(state): State<Arc<AppState>>,
    Path(version): Path<String>,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "releases", "method" => "changes").increment(1);

    let releases = sorted(load(&state).await);
    let Some(this) = releases.iter().find(|r| r.version == version) else {
        return ApiError::not_found(format!("no release {version}"));
    };

    let previous = match q.get("since") {
        Some(want) => match releases.iter().find(|r| &r.version == want) {
            Some(r) => Some(r),
            None => return ApiError::not_found(format!("no release {want} to compare against")),
        },
        // `sorted` is newest first, so the one after this is the one before it.
        None => releases
            .iter()
            .skip_while(|r| r.version != version)
            .nth(1),
    };

    let Some(previous) = previous else {
        // The first release changed everything, and saying so beats an empty
        // diff that reads as "nothing happened".
        return Json(serde_json::json!({
            "version": this.version,
            "since": serde_json::Value::Null,
            "first_release": true,
            "added": this.manifest.iter().map(|m| serde_json::json!({
                "kind": m.kind, "name": m.name,
                "version": m.version, "provenance": m.provenance, "digest": m.digest,
            })).collect::<Vec<_>>(),
            "removed": Vec::<String>::new(),
            "changed": Vec::<String>::new(),
            "unchanged": 0,
        }))
        .into_response();
    };

    let before: std::collections::HashMap<&str, &ManifestEntry> =
        previous.manifest.iter().map(|m| (m.name.as_str(), m)).collect();
    let after: std::collections::HashMap<&str, &ManifestEntry> =
        this.manifest.iter().map(|m| (m.name.as_str(), m)).collect();

    let mut changed = Vec::new();
    let mut added = Vec::new();
    let mut unchanged = 0usize;
    for m in &this.manifest {
        match before.get(m.name.as_str()) {
            None => added.push(serde_json::json!({
                "kind": m.kind, "name": m.name,
                "version": m.version, "provenance": m.provenance, "digest": m.digest,
            })),
            Some(old) if old.digest != m.digest => changed.push(ComponentChange {
                kind: m.kind.clone(),
                name: m.name.clone(),
                from_version: old.version.clone(),
                to_version: m.version.clone(),
                from_provenance: old.provenance.clone(),
                to_provenance: m.provenance.clone(),
                from_digest: old.digest.clone(),
                to_digest: m.digest.clone(),
                notes: m.notes.clone(),
            }),
            Some(_) => unchanged += 1,
        }
    }
    let removed: Vec<_> = previous
        .manifest
        .iter()
        .filter(|m| !after.contains_key(m.name.as_str()))
        .map(|m| serde_json::json!({
            "kind": m.kind, "name": m.name,
            "version": m.version, "provenance": m.provenance, "digest": m.digest,
        }))
        .collect();

    changed.sort_by(|a, b| a.name.cmp(&b.name));
    Json(serde_json::json!({
        "version": this.version,
        "since": previous.version,
        "changed": changed,
        "added": added,
        "removed": removed,
        "unchanged": unchanged,
    }))
    .into_response()
}

async fn notes(State(state): State<Arc<AppState>>, Path(version): Path<String>) -> Response {
    let releases = load(&state).await;
    match releases.iter().find(|r| r.version == version) {
        Some(r) => match &r.notes {
            Some(n) => (
                [(header::CONTENT_TYPE, "text/markdown; charset=utf-8")],
                n.clone(),
            )
                .into_response(),
            None => ApiError::not_found(format!("release {version} has no notes")),
        },
        None => ApiError::not_found(format!("no release {version}")),
    }
}

/// Stream the image straight out of the volume.
///
/// Nothing is staged: the bytes a caller downloads are read from the volume the
/// release names, so the file they get is the image the engine is serving over
/// NVMe/TCP at the same moment, not a copy that may have drifted from it.
async fn download(
    State(state): State<Arc<AppState>>,
    Path(version): Path<String>,
    headers: axum::http::HeaderMap,
) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "releases", "method" => "download").increment(1);

    let releases = load(&state).await;
    let Some(release) = releases.into_iter().find(|r| r.version == version) else {
        return ApiError::not_found(format!("no release {version}"));
    };

    let device = {
        let vm = state.volume_manager.lock().await;
        vm.get_volume(&VolumeId(release.volume_id))
    };
    let Some(device) = device else {
        return ApiError::not_found(format!(
            "release {version} names volume {}, which is not attached",
            release.volume
        ));
    };

    let capacity = device.capacity_bytes();

    // A 32 GB download that cannot resume is one dropped connection away from
    // starting over, so a range is honoured rather than ignored. Ignoring it is
    // legal — the client gets 200 and the whole image — but it is legal in the
    // way that is worst for the caller: they asked for a slice and are handed
    // thirty-two gigabytes.
    let (start, end) = match headers
        .get(header::RANGE)
        .and_then(|v| v.to_str().ok())
        .map(|raw| parse_range(raw, capacity))
    {
        None | Some(RangeSpec::Whole) => (0, capacity.saturating_sub(1)),
        Some(RangeSpec::Partial { start, end }) => (start, end),
        Some(RangeSpec::Unsatisfiable) => {
            return (
                StatusCode::RANGE_NOT_SATISFIABLE,
                [(header::CONTENT_RANGE, format!("bytes */{capacity}"))],
                format!("the image is {capacity} bytes"),
            )
                .into_response();
        }
    };
    let partial = (start, end) != (0, capacity.saturating_sub(1));
    let length = end.saturating_sub(start) + 1;

    let label = release.version.clone();
    let stream = futures_util::stream::unfold((device, start), move |(dev, offset)| {
        let label = label.clone();
        async move {
            if offset > end {
                return None;
            }
            let len = CHUNK.min(end - offset + 1) as usize;
            let mut buf = vec![0u8; len];
            match dev.read(offset, &mut buf).await {
                Ok(_) => Some((
                    Ok::<_, std::io::Error>(bytes::Bytes::from(buf)),
                    (dev, offset + len as u64),
                )),
                Err(e) => {
                    // Ending the body early is the only signal available
                    // mid-stream; say why on the way out so it is not a silent
                    // truncation.
                    tracing::error!(
                        version = %label, offset,
                        "release download failed while reading the volume: {e}"
                    );
                    Some((Err(std::io::Error::other(e.to_string())), (dev, end + 1)))
                }
            }
        }
    });

    let mut hdrs = axum::http::HeaderMap::new();
    let set = |h: &mut axum::http::HeaderMap, k: header::HeaderName, v: String| {
        if let Ok(val) = axum::http::HeaderValue::from_str(&v) {
            h.insert(k, val);
        }
    };
    set(&mut hdrs, header::CONTENT_TYPE, "application/octet-stream".into());
    set(&mut hdrs, header::CONTENT_LENGTH, length.to_string());
    set(&mut hdrs, header::ACCEPT_RANGES, "bytes".into());
    set(
        &mut hdrs,
        header::CONTENT_DISPOSITION,
        format!("attachment; filename=\"stormcos-{version}.img\""),
    );
    let status = if partial {
        set(
            &mut hdrs,
            header::CONTENT_RANGE,
            format!("bytes {start}-{end}/{capacity}"),
        );
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };

    (status, hdrs, Body::from_stream(stream)).into_response()
}

/// What a `Range` header asked for, once resolved against the real size.
#[derive(Debug, PartialEq, Eq)]
enum RangeSpec {
    /// No usable range, or one covering everything: serve 200.
    Whole,
    /// Inclusive byte offsets, both already clamped to the image.
    Partial { start: u64, end: u64 },
    /// Asked for something that is not there: 416.
    Unsatisfiable,
}

/// Parse a single `bytes=` range.
///
/// Only one range is honoured. Multi-range replies are a multipart body that
/// nothing downloading a disk image asks for, and answering the first range of
/// several would be a quiet lie about what was sent — so a multi-range request
/// falls back to the whole image, which is what RFC 9110 permits and what a
/// caller can actually use.
fn parse_range(raw: &str, total: u64) -> RangeSpec {
    if total == 0 {
        return RangeSpec::Unsatisfiable;
    }
    let Some(spec) = raw.trim().strip_prefix("bytes=") else {
        return RangeSpec::Whole;
    };
    if spec.contains(',') {
        return RangeSpec::Whole;
    }
    let Some((from, to)) = spec.split_once('-') else {
        return RangeSpec::Whole;
    };
    let (from, to) = (from.trim(), to.trim());

    let (start, end) = if from.is_empty() {
        // `bytes=-N`: the last N bytes. N of 0 asks for nothing.
        let Ok(n) = to.parse::<u64>() else { return RangeSpec::Whole };
        if n == 0 {
            return RangeSpec::Unsatisfiable;
        }
        (total.saturating_sub(n), total - 1)
    } else {
        let Ok(start) = from.parse::<u64>() else { return RangeSpec::Whole };
        if start >= total {
            return RangeSpec::Unsatisfiable;
        }
        let end = if to.is_empty() {
            total - 1
        } else {
            match to.parse::<u64>() {
                Ok(e) => e.min(total - 1),
                Err(_) => return RangeSpec::Whole,
            }
        };
        if end < start {
            return RangeSpec::Unsatisfiable;
        }
        (start, end)
    };

    if start == 0 && end == total - 1 {
        RangeSpec::Whole
    } else {
        RangeSpec::Partial { start, end }
    }
}

/// Whether a version can be a version.
///
/// It ends up in a URL path and in a filename, so the things that would make it
/// stop being one path segment are refused here rather than discovered later by
/// whatever is asked to serve it.
fn check_version(version: &str) -> Result<(), &'static str> {
    let v = version.trim();
    if v.is_empty() {
        return Err("a release needs a version");
    }
    if v != version {
        return Err("a version may not begin or end with whitespace");
    }
    if v.contains('/') || v.contains('\\') || v.contains("..") {
        return Err("a version may not contain '/', '\\' or '..'");
    }
    if v.starts_with('.') {
        return Err("a version may not begin with '.'");
    }
    if v.chars().any(|c| c.is_control()) {
        return Err("a version may not contain control characters");
    }
    Ok(())
}

async fn publish(State(state): State<Arc<AppState>>, Json(req): Json<PublishRequest>) -> Response {
    metrics::counter!("stormblock_api_requests_total", "endpoint" => "releases", "method" => "publish").increment(1);

    if let Err(why) = check_version(&req.version) {
        return ApiError::bad_request(why);
    }

    let (volume_id, volume_name) = {
        let vm = state.volume_manager.lock().await;
        match vm.find_volume(&req.volume).await {
            Some(id) => {
                let name = vm
                    .list_volumes()
                    .await
                    .into_iter()
                    .find(|(vid, ..)| *vid == id)
                    .map(|(_, n, _, _)| n)
                    .unwrap_or_else(|| req.volume.clone());
                (id, name)
            }
            None => return ApiError::not_found(format!("no volume {}", req.volume)),
        }
    };

    let created_unix = req.created_unix.unwrap_or_else(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0)
    });

    let demote_previous = req.demote_previous.clone();
    let release = Release {
        version: req.version.clone(),
        volume: volume_name,
        volume_id: volume_id.0,
        digest: req.digest,
        created_unix,
        notes: req.notes,
        manifest: req.manifest,
    };

    let mut releases = load(&state).await;
    // Republishing a version replaces its record rather than adding a second:
    // two rows with one version is an index nobody can read.
    releases.retain(|r| r.version != release.version);
    releases.push(release.clone());
    if let Err(e) = save(&state, &releases).await {
        return ApiError::internal(format!("failed to record the release: {e}"));
    }

    tracing::info!(
        version = %release.version, volume = %release.volume,
        components = release.manifest.len(), "published a release"
    );

    // Demote what this one replaces, if the publisher asked for it. Shared
    // extents are copied rather than moved, so what the new image still uses
    // stays on the fast tier and what only the old one used gives its space
    // back — which is the whole point of doing it here rather than by hand.
    let mut demoting = serde_json::Value::Null;
    if let Some(ref tier_name) = demote_previous {
        let Some(tier) = super::slabs::parse_tier(tier_name) else {
            return ApiError::bad_request(format!(
                "invalid demote_previous tier '{tier_name}' (hot, warm, cool, cold)"
            ));
        };
        let previous = sorted(releases.clone())
            .into_iter()
            .find(|r| r.version != release.version && r.volume_id != release.volume_id);
        if let Some(prev) = previous {
            demoting = serde_json::json!({
                "version": prev.version,
                "volume": prev.volume,
                "to_tier": tier.to_string(),
            });
            tracing::info!(
                superseded = %prev.version, by = %release.version, tier = %tier,
                "demoting the release this one replaces"
            );
            let vm_handle = state.volume_manager.clone();
            let prev_id = VolumeId(prev.volume_id);
            tokio::spawn(async move {
                let mut vm = vm_handle.lock().await;
                if let Err(e) = vm.retier_volume(prev_id, tier).await {
                    tracing::error!(volume = %prev_id, "demotion failed: {e}");
                }
            });
        }
    }

    let mut body = serde_json::to_value(summarise(&state, &release).await)
        .unwrap_or_else(|_| serde_json::json!({}));
    if let Some(obj) = body.as_object_mut() {
        obj.insert("demoting".into(), demoting);
    }
    (StatusCode::CREATED, Json(body)).into_response()
}

async fn unpublish(State(state): State<Arc<AppState>>, Path(version): Path<String>) -> Response {
    let mut releases = load(&state).await;
    let before = releases.len();
    releases.retain(|r| r.version != version);
    if releases.len() == before {
        return ApiError::not_found(format!("no release {version}"));
    }
    if let Err(e) = save(&state, &releases).await {
        return ApiError::internal(format!("failed to update the index: {e}"));
    }
    // The volume is left alone. Withdrawing a release says "stop offering
    // this", not "destroy it": the two are different decisions and only one of
    // them is reversible.
    tracing::info!(version = %version, "withdrew a release; its volume is untouched");
    StatusCode::NO_CONTENT.into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index).post(publish))
        .route("/index.html", get(index_html))
        .route("/{version}", get(get_one).delete(unpublish))
        .route("/{version}/manifest", get(manifest))
        .route("/{version}/notes", get(notes))
        .route("/{version}/changes", get(changes))
        .route("/{version}/image.img", get(download))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The index shows a date, not a number of seconds, and the conversion is
    /// the one place that could quietly be wrong for years.
    #[test]
    fn dates_render() {
        assert_eq!(civil_date(0), "1970-01-01T00:00:00Z");
        assert_eq!(civil_date(1_756_800_000), "2025-09-02T08:00:00Z");
        // A leap day, which is where a hand-rolled calendar goes wrong.
        assert_eq!(civil_date(1_709_164_800), "2024-02-29T00:00:00Z");
    }

    const TOTAL: u64 = 34_359_738_368;

    /// The ordinary cases a downloader sends.
    #[test]
    fn ranges_resolve() {
        assert_eq!(
            parse_range("bytes=0-99", TOTAL),
            RangeSpec::Partial { start: 0, end: 99 }
        );
        // Open-ended: resume from where the last attempt stopped.
        assert_eq!(
            parse_range("bytes=1048576-", TOTAL),
            RangeSpec::Partial { start: 1_048_576, end: TOTAL - 1 }
        );
        // Suffix: the last N bytes, which is how a backup GPT gets read.
        assert_eq!(
            parse_range("bytes=-512", TOTAL),
            RangeSpec::Partial { start: TOTAL - 512, end: TOTAL - 1 }
        );
        // An end past the image is clamped, not refused.
        assert_eq!(
            parse_range("bytes=0-99999999999999", TOTAL),
            RangeSpec::Whole
        );
        assert_eq!(
            parse_range("bytes=10-99999999999999", TOTAL),
            RangeSpec::Partial { start: 10, end: TOTAL - 1 }
        );
    }

    /// A range covering the whole thing is served as 200, not as a 206 that
    /// claims to be a slice of itself.
    #[test]
    fn a_range_over_everything_is_not_partial() {
        assert_eq!(parse_range("bytes=0-", TOTAL), RangeSpec::Whole);
        assert_eq!(parse_range(&format!("bytes=0-{}", TOTAL - 1), TOTAL), RangeSpec::Whole);
        assert_eq!(parse_range(&format!("bytes=-{TOTAL}"), TOTAL), RangeSpec::Whole);
    }

    /// Asking for something that is not there earns a 416, and everything the
    /// parser cannot make sense of falls back to the whole image rather than
    /// to a guess.
    #[test]
    fn unsatisfiable_and_unparseable_are_different() {
        assert_eq!(parse_range("bytes=34359738368-", TOTAL), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=99-10", TOTAL), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=-0", TOTAL), RangeSpec::Unsatisfiable);
        assert_eq!(parse_range("bytes=0-99", 0), RangeSpec::Unsatisfiable);

        for junk in ["", "items=0-99", "bytes=abc-def", "bytes=", "bytes=1-2-3"] {
            assert_eq!(parse_range(junk, TOTAL), RangeSpec::Whole, "{junk:?}");
        }
        // Several ranges at once: answered whole rather than with the first of
        // them, which would be a quiet lie about what was sent.
        assert_eq!(parse_range("bytes=0-99,200-299", TOTAL), RangeSpec::Whole);
    }

    fn entry(name: &str, digest: &str, version: &str) -> ManifestEntry {
        ManifestEntry {
            kind: "binary".into(),
            name: name.into(),
            digest: digest.into(),
            size_bytes: None,
            provenance: Some(format!("{name}@abc1234")),
            version: Some(version.into()),
            notes: None,
        }
    }

    /// A rebuild is not news. Comparison is by digest, so a component built
    /// again from the same source with the same bytes must not appear in a
    /// change list — a release note padded with things that did not change is
    /// one nobody reads twice.
    #[test]
    fn only_a_different_digest_counts_as_a_change() {
        let old = vec![
            entry("stormblock", "aaaa", "13.3.0"),
            entry("rustkube", "bbbb", "0.7.35"),
            entry("gone", "cccc", "1.0.0"),
        ];
        let new = vec![
            entry("stormblock", "dddd", "13.4.0"),
            entry("rustkube", "bbbb", "0.7.35"),
            entry("arrived", "eeee", "0.1.0"),
        ];

        let before: std::collections::HashMap<&str, &ManifestEntry> =
            old.iter().map(|m| (m.name.as_str(), m)).collect();
        let after: std::collections::HashMap<&str, &ManifestEntry> =
            new.iter().map(|m| (m.name.as_str(), m)).collect();

        let changed: Vec<&str> = new
            .iter()
            .filter(|m| before.get(m.name.as_str()).is_some_and(|o| o.digest != m.digest))
            .map(|m| m.name.as_str())
            .collect();
        let added: Vec<&str> = new
            .iter()
            .filter(|m| !before.contains_key(m.name.as_str()))
            .map(|m| m.name.as_str())
            .collect();
        let removed: Vec<&str> = old
            .iter()
            .filter(|m| !after.contains_key(m.name.as_str()))
            .map(|m| m.name.as_str())
            .collect();
        let unchanged = new
            .iter()
            .filter(|m| before.get(m.name.as_str()).is_some_and(|o| o.digest == m.digest))
            .count();

        assert_eq!(changed, vec!["stormblock"], "only the one whose bytes moved");
        assert_eq!(added, vec!["arrived"]);
        assert_eq!(removed, vec!["gone"]);
        assert_eq!(unchanged, 1, "rustkube was rebuilt to the same bytes");
    }

    /// A version is a path segment and a filename, so anything that would stop
    /// it being one is refused where it is published rather than wherever it is
    /// later interpreted.
    #[test]
    fn a_version_cannot_escape_its_url() {
        for good in ["10.21", "10.21-rc1", "2026.09.02"] {
            assert!(check_version(good).is_ok(), "{good} should be allowed");
        }
        for bad in ["", "  ", "..", "../etc/passwd", "10/21", ".hidden", "10.21 ", "a\u{7f}b"] {
            assert!(check_version(bad).is_err(), "{bad:?} should be refused");
        }
    }
}
