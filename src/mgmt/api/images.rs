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

async fn build(State(_state): State<Arc<AppState>>, Json(req): Json<BuildRequest>) -> Response {
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

    let out_path = PathBuf::from(&req.out);
    if !out_path.is_absolute() {
        return ApiError::bad_request(format!(
            "out is the relative path {}, and a daemon has no directory to resolve it against",
            out_path.display()
        ));
    }
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

async fn convert(State(_state): State<Arc<AppState>>, Json(req): Json<ConvertRequest>) -> Response {
    let format = match format_of(&req.out, &req.format) {
        Ok(f) => f,
        Err(m) => return ApiError::bad_request(m),
    };
    let r = if format == ImageFormat::Iso {
        iso::from_image_with(
            Path::new(&req.input),
            Path::new(&req.out),
            iso::IsoOptions {
                include_slab: req.include_slab,
            },
        )
        .await
    } else {
        formats::convert(Path::new(&req.input), Path::new(&req.out), format).await
    };
    match r {
        Err(e) => err(e),
        Ok(_) => {
            let len = tokio::fs::metadata(&req.out)
                .await
                .map(|m| m.len())
                .unwrap_or(0);
            Json(json!({
                "path": req.out,
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
}
