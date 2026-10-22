//! `/api/v1/images` — build a disk image or an ISO out of pallets.
//!
//! The builder is `crate::image`, a library. The CLI (`stormblock image
//! build --spec …`) is about a hundred lines of glue over it, and so is this:
//! same `ImageBuilder`, same `BuildReport`, same verification. Nothing here
//! is a second implementation, and nothing here is reachable only from a
//! shell.
//!
//! **Why this is engine-level rather than a profile's.** Building an image is
//! not a deployment choice — `docs/layering.md` puts mechanism in the engine
//! precisely so a second profile does not have to fork it. An image file *is*
//! a drive to this engine, so assembly is the ordinary `PalletManager` over a
//! `FileDevice`; a profile that serves volumes gets this route by merging
//! `mgmt::api::router`, and adds nothing.
//!
//! **Paths are resolved, never `chdir`-ed.** The CLI changes directory so a
//! spec's relative paths resolve against the spec file, which is right for a
//! process that then exits. A daemon cannot: the working directory is
//! process-global, so a build would move the ground under every other request
//! in flight. Relative paths are resolved against `base_dir` (or the spec
//! file's own directory) and refused when there is nothing to resolve them
//! against, which is a diagnostic rather than a surprise.
//!
//! **A build holds its connection.** Assembling and verifying an image is
//! minutes of I/O, not milliseconds. That is the same bargain the CLI makes,
//! and it keeps "did it verify" in the same answer as "did it build" — a job
//! id would separate them.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::{
    extract::State,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use serde_json::json;

use super::ApiError;
use crate::image::{formats, iso, ImageBuilder, ImageError, ImageFormat, ImageSpec};
use crate::mgmt::AppState;

fn err(e: ImageError) -> Response {
    match e {
        ImageError::Spec(m) => ApiError::bad_request(m),
        ImageError::TooSmall { .. } => ApiError::bad_request(e.to_string()),
        other => ApiError::internal(other.to_string()),
    }
}

// ------------------------------------------------------------------ build

#[derive(Debug, Deserialize)]
pub struct BuildRequest {
    /// The spec as JSON. `ImageSpec` is `Deserialize`, so this is the same
    /// document the TOML describes, in the transport this API speaks.
    #[serde(default)]
    pub spec: Option<ImageSpec>,
    /// Or the TOML itself, for a caller that already has one.
    #[serde(default)]
    pub spec_toml: Option<String>,
    /// Or a spec file on this node. Its directory becomes `base_dir` unless
    /// one is given.
    #[serde(default)]
    pub spec_path: Option<String>,
    /// What a relative path in the spec is relative to.
    #[serde(default)]
    pub base_dir: Option<String>,
    /// Where to write the image.
    pub out: String,
    /// `raw` | `qcow2` | `vhd` | `vmdk` | `iso`. Inferred from `out` when
    /// absent.
    #[serde(default)]
    pub format: Option<String>,
    /// Keep the intermediate raw image beside a converted one.
    #[serde(default)]
    pub keep_raw: bool,
    /// ISO only: carry the slab. Off by default, because a fresh slab is
    /// empty and carrying it turns a 35 MB image into a 320 MB one of zeros.
    #[serde(default)]
    pub include_slab: bool,
}

#[derive(Debug, Serialize)]
pub struct BuildResponse {
    pub path: String,
    pub format: String,
    pub size_bytes: u64,
    pub block_size: u32,
    pub partitions: Vec<crate::image::build::PartitionReport>,
    /// The raw image, when a conversion produced a different file and it was
    /// kept.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub raw_path: Option<String>,
}

/// Resolve the spec from whichever of the three forms the caller sent.
async fn spec_of(req: &BuildRequest) -> Result<(ImageSpec, Option<PathBuf>), ImageError> {
    if let Some(s) = &req.spec {
        return Ok((s.clone(), None));
    }
    if let Some(t) = &req.spec_toml {
        return ImageSpec::from_toml(t).map(|s| (s, None));
    }
    if let Some(p) = &req.spec_path {
        return ImageSpec::load(p)
            .await
            .map(|s| (s, Path::new(p).parent().map(PathBuf::from)));
    }
    Err(ImageError::Spec(
        "give one of spec (JSON), spec_toml, or spec_path".into(),
    ))
}

/// Make every path in a spec absolute, or say which one could not be.
///
/// A relative path that silently resolved against whatever directory the
/// daemon happens to be in would build an image out of the wrong files — or,
/// worse, out of files that happen to exist.
fn absolutize(spec: &mut ImageSpec, base: Option<&Path>) -> Result<(), String> {
    fn fix(p: &mut PathBuf, base: Option<&Path>, what: &str) -> Result<(), String> {
        if p.is_absolute() {
            return Ok(());
        }
        match base {
            Some(b) => {
                *p = b.join(&*p);
                Ok(())
            }
            None => Err(format!(
                "{what} is the relative path {}, and there is nothing to resolve it against — \
                 send absolute paths, or set base_dir",
                p.display()
            )),
        }
    }

    if let Some(esp) = spec.esp.as_mut() {
        if let Some(p) = esp.from_dir.as_mut() {
            fix(p, base, "esp.from_dir")?;
        }
        if let Some(p) = esp.from_image.as_mut() {
            fix(p, base, "esp.from_image")?;
        }
    }
    for (i, pal) in spec.pallets.iter_mut().enumerate() {
        if let Some(p) = pal.from_image.as_mut() {
            fix(p, base, &format!("pallet[{i}].from_image"))?;
        }
        for m in pal.members.iter_mut() {
            if let Some(p) = m.file.as_mut() {
                fix(p, base, &format!("pallet[{i}].member {}", m.name))?;
            }
        }
    }
    for (i, part) in spec.partitions.iter_mut().enumerate() {
        if let Some(p) = part.from_file.as_mut() {
            fix(p, base, &format!("partition[{i}].from_file"))?;
        }
    }
    Ok(())
}

/// The format the caller asked for, or the one its filename implies. A name
/// that is not a format is refused rather than quietly built as raw: an ISO
/// that came back as a raw image would not be discovered until it failed to
/// boot.
fn format_of(out: &str, want: &Option<String>) -> Result<ImageFormat, String> {
    match want {
        Some(f) => ImageFormat::parse(f).ok_or_else(|| format!("unknown image format '{f}'")),
        None => Ok(ImageFormat::from_path(Path::new(out)).unwrap_or(ImageFormat::Raw)),
    }
}

/// Where an image may be written (#393), or why not. The route writes as
/// the engine's own user, so a path the caller names is never taken on trust:
///
/// - a ublk device this engine created by an attach (`/dev/ublkbN`, a
///   volume of its own), raw only — a conversion writes a raw image beside
///   its output, and there is no "beside" a device — and not mounted. A boot
///   device the engine adopted (the root, the node's own volumes) is not one;
/// - else a file inside the images directory (`[images] dir`, by default
///   `<data_dir>/images`): no `..`, its parent resolved through every link
///   and still inside, and no link, device or directory already at the name.
///
/// The answer is the path to write — the parent's resolved spelling, so a
/// link swapped into the parent afterwards does not move the write — and
/// whether it is a device.
async fn allowed_out(state: &AppState, out: &Path, format: ImageFormat) -> Result<(PathBuf, bool), String> {
    let shown = out.display();
    if !out.is_absolute() {
        return Err(format!(
            "out is the relative path {shown}, and a daemon has no directory to resolve it against"
        ));
    }
    let attached = state.ublk_exports.lock().await.attached_volume_at(&out.to_string_lossy());
    if let Some(volume) = attached {
        if format != ImageFormat::Raw {
            return Err(format!(
                "out {shown} is volume {volume}'s ublk device: only a raw image is written onto a device \
                 ({format} converts from a raw image written beside its output)"
            ));
        }
        if let Some(at) = crate::mgmt::ublk_export::mounted_at(&out.to_string_lossy()) {
            return Err(format!("out {shown} is volume {volume}'s ublk device and is mounted on {at}"));
        }
        return Ok((out.to_path_buf(), true));
    }
    let Some(dir) = state.config.images.dir(state.data_dir.as_deref()) else {
        return Err(format!(
            "out {shown} is not a ublk device this engine attached, and there is no images directory \
             (set [images] dir or a data directory)"
        ));
    };
    let outside = || {
        format!(
            "out {shown} is outside the images directory {}: an image is written there, or onto a ublk \
             device this engine attached",
            dir.display()
        )
    };
    if out.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        return Err(outside());
    }
    let (Some(parent), Some(name)) = (out.parent(), out.file_name()) else {
        return Err(outside());
    };
    if let Err(e) = tokio::fs::create_dir_all(&dir).await {
        return Err(format!("the images directory {} cannot be made: {e}", dir.display()));
    }
    let root = tokio::fs::canonicalize(&dir)
        .await
        .map_err(|e| format!("the images directory {}: {e}", dir.display()))?;
    let parent = match tokio::fs::canonicalize(parent).await {
        Ok(p) => p,
        Err(_) if !out.starts_with(&dir) && !out.starts_with(&root) => return Err(outside()),
        Err(e) => return Err(format!("out {shown}: its directory {}: {e}", parent.display())),
    };
    if !parent.starts_with(&root) {
        return Err(outside());
    }
    let path = parent.join(name);
    let mut names = vec![path.clone()];
    if format != ImageFormat::Raw {
        names.push(path.with_extension("raw.img"));
    }
    for p in &names {
        if let Ok(m) = tokio::fs::symlink_metadata(p).await {
            if !m.file_type().is_file() {
                return Err(format!(
                    "{} is already there and is not a plain file (a link, a device or a directory): \
                     not written over",
                    p.display()
                ));
            }
        }
    }
    Ok((path, false))
}

async fn build(State(state): State<Arc<AppState>>, Json(req): Json<BuildRequest>) -> Response {
    let format = match format_of(&req.out, &req.format) {
        Ok(f) => f,
        Err(m) => return ApiError::bad_request(m),
    };
    let (mut spec, spec_dir) = match spec_of(&req).await {
        Ok(v) => v,
        Err(e) => return err(e),
    };
    let base = req
        .base_dir
        .as_ref()
        .map(PathBuf::from)
        .or(spec_dir)
        .filter(|d| !d.as_os_str().is_empty());
    if let Err(e) = absolutize(&mut spec, base.as_deref()) {
        return ApiError::bad_request(e);
    }

    let out_path = match allowed_out(&state, Path::new(&req.out), format).await {
        Ok((p, _)) => p,
        Err(m) => return ApiError::bad_request(m),
    };
    // A conversion reads a finished raw image, so a non-raw format builds
    // beside its output first.
    let raw_path = if format == ImageFormat::Raw {
        out_path.clone()
    } else {
        out_path.with_extension("raw.img")
    };

    tracing::info!(
        "image build: {} → {} ({format}), {} pallet(s)",
        spec.name,
        out_path.display(),
        spec.pallets.len()
    );
    let report = match ImageBuilder::new(spec).build(&raw_path).await {
        Ok(r) => r,
        Err(e) => return err(e),
    };

    let mut size_bytes = report.size_bytes;
    if format != ImageFormat::Raw {
        let converted = if format == ImageFormat::Iso {
            iso::from_image_with(
                &raw_path,
                &out_path,
                iso::IsoOptions {
                    include_slab: req.include_slab,
                },
            )
            .await
        } else {
            formats::convert(&raw_path, &out_path, format).await
        };
        if let Err(e) = converted {
            return err(e);
        }
        size_bytes = tokio::fs::metadata(&out_path)
            .await
            .map(|m| m.len())
            .unwrap_or(size_bytes);
        if !req.keep_raw {
            let _ = tokio::fs::remove_file(&raw_path).await;
        }
    }

    Json(BuildResponse {
        path: out_path.display().to_string(),
        format: format.as_str().to_string(),
        size_bytes,
        block_size: report.block_size,
        partitions: report.partitions,
        raw_path: (format != ImageFormat::Raw && req.keep_raw)
            .then(|| raw_path.display().to_string()),
    })
    .into_response()
}

// ---------------------------------------------------------------- convert

#[derive(Debug, Deserialize)]
pub struct ConvertRequest {
    pub input: String,
    pub out: String,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub include_slab: bool,
}

async fn convert(State(state): State<Arc<AppState>>, Json(req): Json<ConvertRequest>) -> Response {
    let format = match format_of(&req.out, &req.format) {
        Ok(f) => f,
        Err(m) => return ApiError::bad_request(m),
    };
    // Same places as a build (#393). A conversion writes its output alone,
    // so the raw-beside rule of a device does not apply: a raw "conversion"
    // onto an attached device is a copy.
    let out = match allowed_out(&state, Path::new(&req.out), ImageFormat::Raw).await {
        Ok((p, device)) if format == ImageFormat::Raw || !device => p,
        Ok(_) => return ApiError::bad_request(format!("out {} is a device: only a raw image is written onto one", req.out)),
        Err(m) => return ApiError::bad_request(m),
    };
    let r = if format == ImageFormat::Iso {
        iso::from_image_with(
            Path::new(&req.input),
            &out,
            iso::IsoOptions {
                include_slab: req.include_slab,
            },
        )
        .await
    } else {
        formats::convert(Path::new(&req.input), &out, format).await
    };
    match r {
        Err(e) => err(e),
        Ok(_) => {
            let len = tokio::fs::metadata(&out)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            Json(json!({
                "path": out.display().to_string(),
                "format": format.as_str(),
                "size_bytes": len,
            }))
            .into_response()
        }
    }
}

// ---------------------------------------------------------------- inspect

#[derive(Debug, Deserialize)]
pub struct InspectRequest {
    pub path: String,
}

/// What is in an image — the GPT, and the pallets, read through the ordinary
/// pallet tooling. An image and a drive are the same object here, which is
/// the property that makes this worth having: the answer is the same one you
/// would get by pointing this at `/dev/nvme0n1`.
async fn inspect(State(_state): State<Arc<AppState>>, Json(req): Json<InspectRequest>) -> Response {
    let path = Path::new(&req.path);
    let gpt = match crate::image::build::table_of(path).await {
        Ok(g) => g,
        Err(e) => return err(e),
    };
    let partitions: Vec<_> = gpt
        .partitions()
        .map(|(i, e)| {
            json!({
                "index": i,
                "name": e.name,
                "start_bytes": e.start_bytes(gpt.block_size),
                "size_bytes": e.size_bytes(gpt.block_size),
                "is_pallet": e.is_pallet(),
            })
        })
        .collect();
    let pallets = match crate::image::build::pallets_in(path).await {
        Ok(p) => p,
        Err(e) => return err(e),
    };
    let pallets: Vec<_> = pallets
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "name": p.name,
                "kind": p.kind.to_string(),
                "version": p.version,
                "version_label": p.version_label,
                "member_count": p.member_count,
                "start_bytes": p.start_bytes,
                "size_bytes": p.size_bytes,
                "used_bytes": p.used_bytes,
                "readable": p.is_readable(),
            })
        })
        .collect();
    Json(json!({
        "path": req.path,
        "block_size": gpt.block_size,
        "recovered_from_backup": gpt.recovered_from_backup,
        "partitions": partitions,
        "pallets": pallets,
    }))
    .into_response()
}

// ---------------------------------------------------------------- formats

async fn list_formats() -> Response {
    let items: Vec<_> = ImageFormat::ALL
        .iter()
        .map(|f| json!({"format": f.as_str(), "extension": f.extension()}))
        .collect();
    Json(json!({"items": items, "count": items.len()})).into_response()
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/build", post(build))
        .route("/convert", post(convert))
        .route("/inspect", post(inspect))
        .route("/formats", get(list_formats))
        .with_state(state)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::{EspSpec, MemberEntry, PalletEntry};

    fn spec_with(file: &str, esp: &str) -> ImageSpec {
        ImageSpec {
            name: "t".into(),
            esp: Some(EspSpec {
                from_dir: Some(PathBuf::from(esp)),
                ..Default::default()
            }),
            pallets: vec![PalletEntry {
                name: Some("boot".into()),
                members: vec![MemberEntry {
                    name: "kernel".into(),
                    role: "kernel".into(),
                    file: Some(PathBuf::from(file)),
                    ..Default::default()
                }],
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    #[test]
    fn relative_paths_resolve_against_the_base_and_absolute_ones_are_left_alone() {
        let mut s = spec_with("build/vmlinuz", "/already/absolute");
        absolutize(&mut s, Some(Path::new("/srv/specs"))).unwrap();
        assert_eq!(
            s.pallets[0].members[0].file.as_ref().unwrap(),
            Path::new("/srv/specs/build/vmlinuz")
        );
        assert_eq!(
            s.esp.as_ref().unwrap().from_dir.as_ref().unwrap(),
            Path::new("/already/absolute")
        );
    }

    /// The alternative is a daemon that resolves against whatever directory
    /// it happens to be in, and builds an image out of files nobody named.
    #[test]
    fn a_relative_path_with_no_base_is_refused_and_says_which_one() {
        let mut s = spec_with("build/vmlinuz", "/abs");
        let e = absolutize(&mut s, None).unwrap_err();
        assert!(e.contains("kernel"), "{e}");
        assert!(e.contains("base_dir"), "{e}");
    }

    #[test]
    fn the_format_comes_from_the_request_or_the_extension() {
        assert_eq!(format_of("/out/x.iso", &None).unwrap(), ImageFormat::Iso);
        assert_eq!(
            format_of("/out/x.iso", &Some("qcow2".into())).unwrap(),
            ImageFormat::Qcow2
        );
        // No extension and no request: raw, which is what a bare file is.
        assert_eq!(
            format_of("/out/disk", &None).ok().unwrap(),
            ImageFormat::Raw
        );
        assert!(format_of("/out/x", &Some("qcow3".into())).is_err());
    }

    async fn state_in(data_dir: Option<&Path>, images: Option<&Path>) -> AppState {
        let mut config = crate::mgmt::config::StormBlockConfig::default();
        config.management.data_dir = data_dir.map(|d| d.to_string_lossy().to_string());
        config.images.dir = images.map(|d| d.to_string_lossy().to_string());
        let vm = crate::volume::VolumeManager::new(1 << 20);
        let (reg, gem) = (vm.registry().clone(), vm.gem().clone());
        AppState::new(config, vm, reg, gem)
    }

    /// #393: a file goes inside the images directory and nowhere else; each
    /// way out of it is refused and says where an image may go.
    #[tokio::test]
    async fn an_image_file_is_written_inside_the_images_directory_only() {
        let tmp = tempfile::TempDir::new().unwrap();
        let data = tmp.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let st = state_in(Some(&data), None).await;
        let images = std::fs::canonicalize(&data).unwrap().join("images");

        // Inside (the directory is made on first use).
        let (p, device) = allowed_out(&st, &data.join("images/a.img"), ImageFormat::Raw).await.unwrap();
        assert_eq!((p, device), (images.join("a.img"), false));

        // Anywhere else.
        for out in ["/etc/stormblock-393.img", "/dev/sda"] {
            let e = allowed_out(&st, Path::new(out), ImageFormat::Raw).await.unwrap_err();
            assert!(e.contains("outside the images directory"), "{out}: {e}");
        }
        let e = allowed_out(&st, &tmp.path().join("x.img"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("outside"), "{e}");
        // Relative.
        let e = allowed_out(&st, Path::new("a.img"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("relative"), "{e}");
        // `..` out of it, whether or not it would land back inside.
        for out in [data.join("images/../escape.img"), data.join("images/../images/a.img")] {
            let e = allowed_out(&st, &out, ImageFormat::Raw).await.unwrap_err();
            assert!(e.contains("outside"), "{}: {e}", out.display());
        }
        // A link inside that leads out: its target is where the write would land.
        let away = tmp.path().join("away");
        std::fs::create_dir_all(&away).unwrap();
        std::os::unix::fs::symlink(&away, images.join("sub")).unwrap();
        let e = allowed_out(&st, &images.join("sub/x.img"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("outside"), "{e}");
        // A link at the name itself, and the raw image a conversion writes beside it.
        std::os::unix::fs::symlink("/etc/passwd", images.join("p.img")).unwrap();
        let e = allowed_out(&st, &images.join("p.img"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("not a plain file"), "{e}");
        std::os::unix::fs::symlink("/etc/passwd", images.join("q.raw.img")).unwrap();
        let e = allowed_out(&st, &images.join("q.qcow2"), ImageFormat::Qcow2).await.unwrap_err();
        assert!(e.contains("q.raw.img"), "{e}");
        // A directory at the name.
        std::fs::create_dir_all(images.join("d.img")).unwrap();
        assert!(allowed_out(&st, &images.join("d.img"), ImageFormat::Raw).await.is_err());
        // A plain file already there is rewritten, as a build always has.
        std::fs::write(images.join("old.img"), b"x").unwrap();
        assert!(allowed_out(&st, &images.join("old.img"), ImageFormat::Raw).await.is_ok());
        // A subdirectory that exists is inside too.
        std::fs::create_dir_all(images.join("rel")).unwrap();
        assert!(allowed_out(&st, &images.join("rel/b.iso"), ImageFormat::Iso).await.is_ok());
    }

    /// `[images] dir` names the directory; with neither it nor a data
    /// directory, no file is written at all.
    #[tokio::test]
    async fn the_images_directory_is_configured_or_there_is_none() {
        let tmp = tempfile::TempDir::new().unwrap();
        let mine = tmp.path().join("mine");
        let st = state_in(Some(&tmp.path().join("data")), Some(&mine)).await;
        assert!(allowed_out(&st, &mine.join("a.img"), ImageFormat::Raw).await.is_ok());
        let e = allowed_out(&st, &tmp.path().join("data/images/a.img"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("outside"), "{e}");

        let st = state_in(None, None).await;
        let e = allowed_out(&st, &tmp.path().join("a.img"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("no images directory"), "{e}");
    }

    /// A ublk device this engine attached takes a raw image; a conversion's
    /// format does not go onto one; an adopted boot device (the root) is
    /// never written.
    #[tokio::test]
    async fn an_attached_ublk_device_takes_a_raw_image_and_a_boot_device_never() {
        let tmp = tempfile::TempDir::new().unwrap();
        let st = state_in(Some(tmp.path()), None).await;
        let vol = uuid::Uuid::new_v4().to_string();
        let root = uuid::Uuid::new_v4().to_string();
        {
            let mut u = st.ublk_exports.lock().await;
            u.insert_fake(&vol, "/dev/ublkb93");
            u.record_adopted(&root, "/dev/ublkb0".into());
        }
        let (p, device) = allowed_out(&st, Path::new("/dev/ublkb93"), ImageFormat::Raw).await.unwrap();
        assert_eq!((p, device), (PathBuf::from("/dev/ublkb93"), true));
        let e = allowed_out(&st, Path::new("/dev/ublkb93"), ImageFormat::Iso).await.unwrap_err();
        assert!(e.contains("only a raw image"), "{e}");
        let e = allowed_out(&st, Path::new("/dev/ublkb0"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("outside the images directory"), "{e}");
        let e = allowed_out(&st, Path::new("/dev/ublkb7"), ImageFormat::Raw).await.unwrap_err();
        assert!(e.contains("outside"), "{e}");
    }
}
