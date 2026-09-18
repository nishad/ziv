use std::path::PathBuf;
use std::sync::Arc;

use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use exporter::{dzi, export_with_progress, ExportEvent, ExportOptions};
use tiling::{TileEngine, ZarrTileEngine};
use zarr_core::ZarrImage;

pub mod render;

#[derive(Parser)]
#[command(
    name = "ziv",
    version,
    about = "OME-Zarr IIIF tile server + static exporter",
    long_about = "ziv is a single, zero-install binary that turns an OME-Zarr image into IIIF \
        Image API 3.0 tiles — either as a live HTTP server (`ziv serve`) or a self-contained \
        static tile tree (`ziv export`) you can drop on any static host. Both commands accept a \
        local filesystem path or a remote store URL (s3://, gs://, az://, http(s)://)."
)]
pub struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Serve one or more OME-Zarr images as a live IIIF Image API 3.0 server.
    Serve {
        /// One or more store specs for `.ome.zarr` images: local filesystem paths, or remote URLs
        /// (`s3://bucket/path`, `gs://bucket/path`, `az://container/path`,
        /// `http(s)://host/path`).
        ///
        /// A single image is served at the root exactly as before. Two or more are served under
        /// `/i/{name}/`, where the name is the final path component with a trailing `.ome.zarr` or
        /// `.zarr` removed. Names derived this way change when a file moves, so they are not safe
        /// to cite; see `docs/multi-image.md`.
        ///
        /// Remote credentials are read from the standard env vars for each provider's SDK:
        /// AWS (`s3://`): AWS_ACCESS_KEY_ID, AWS_SECRET_ACCESS_KEY, AWS_SESSION_TOKEN,
        /// AWS_REGION/AWS_DEFAULT_REGION, AWS_ENDPOINT. GCS (`gs://`): GOOGLE_SERVICE_ACCOUNT,
        /// GOOGLE_SERVICE_ACCOUNT_KEY, or any other GOOGLE_* var. Azure (`az://`):
        /// AZURE_STORAGE_ACCOUNT_NAME, AZURE_STORAGE_ACCOUNT_KEY, AZURE_STORAGE_SAS_KEY, or
        /// any other AZURE_* var. `http(s)://` stores take no credentials.
        #[arg(required = true, num_args = 1..)]
        paths: Vec<String>,
        /// Address to bind.
        #[arg(long, default_value = "127.0.0.1:3000")]
        addr: String,
        /// Fail startup unless auth is configured. Auth itself is always read from
        /// `ZIV_AUTH_BEARER`/`ZIV_AUTH_HMAC_SECRET` env vars (never a CLI flag — see those vars'
        /// docs); this flag only turns "neither var set" into a startup error instead of quietly
        /// serving unauthenticated, for deployments that want to fail loud on a missing secret
        /// rather than accidentally serve in the open.
        #[arg(long)]
        require_auth: bool,
        /// Public origin (e.g. `https://images.example.org`) to embed in the IIIF `id` (and any
        /// HMAC-signed URLs) instead of the bind address. Set this when ziv sits behind a
        /// TLS-terminating reverse proxy so `info.json`'s `id` reflects the address clients
        /// actually use, not `--addr`. Also settable via `ZIV_PUBLIC_BASE_URL` (this flag wins if
        /// both are set). If neither is set, ziv falls back to honoring per-request
        /// `X-Forwarded-Proto`/`X-Forwarded-Host` headers when present, then finally the bind
        /// address — see `server::origin` for the full precedence and trust considerations of
        /// honoring forwarded headers by default.
        #[arg(long)]
        public_base_url: Option<String>,
        /// Disable the SSRF guard on `http(s)://` store specs, permitting loopback/link-local
        /// (including the cloud metadata endpoint 169.254.169.254)/private-network hosts. OFF
        /// by default (secure by default) — only set this for a legitimate internal deployment
        /// (e.g. an in-VPC MinIO reachable at a private IP). Also settable via
        /// `ZIV_ALLOW_INTERNAL_HOSTS` (either one being set enables it). See
        /// `zarr_core::store::ssrf` for the exact blocked ranges.
        #[arg(long)]
        allow_internal_hosts: bool,
    },
    /// Export an OME-Zarr image as a self-contained static IIIF Image API 3.0 Level-0
    /// tile tree: every tile file OpenSeadragon v3 will request, a level0 info.json that
    /// pins the finite request space, and an embedded viewer. Drop the output directory
    /// on any static host (S3/GitHub Pages/CDN) for a live zoomable image with zero
    /// backend.
    Export {
        /// Store spec for the .ome.zarr image: a local filesystem path, or a remote URL
        /// (`s3://bucket/path`, `gs://bucket/path`, `az://container/path`,
        /// `http(s)://host/path`). See `serve --help` for remote credential env vars.
        src: String,
        /// Output directory (created if missing). `info.json` and `index.html` are
        /// written at its root; tiles under `{region}/{size}/0/default.jpg`.
        out_dir: PathBuf,
        /// The IIIF id of the root tree. Defaults to . (relative to the export root). With
        /// --planes or --labels it is also the base of every other tree's id: pass the URL the
        /// export will be hosted at for absolute, standard ids. A trailing slash is trimmed (a
        /// IIIF id must not carry one), and an empty string is rewritten to ".".
        #[arg(long)]
        id: Option<String>,
        /// JPEG quality (1-100) used for every rendered tile.
        #[arg(long, default_value_t = 85)]
        quality: u8,
        /// Also export every z-plane, each as its own Level-0 tile tree under `planes/{z}/`, at
        /// the default timepoint and channels. The exported viewer gets a z slider.
        #[arg(long)]
        planes: bool,
        /// Also export an overlay of every label image on each exported plane, under
        /// `planes/{z}/labels/{i}/`, in distinct colours. The exported viewer gets a label picker.
        #[arg(long)]
        labels: bool,
        /// Opacity of exported label overlays, 0 to 1.
        #[arg(long, default_value_t = exporter::DEFAULT_OVERLAY_OPACITY)]
        overlay_opacity: f64,
        /// Also emit a DeepZoom (DZI) tree alongside the IIIF export, sharing the same
        /// raster: `{name}.dzi` + `{name}_files/{level}/{col}_{row}.jpg`.
        #[arg(long)]
        dzi: bool,
        /// Base name for the DZI descriptor/tile directory when `--dzi` is set.
        #[arg(long, default_value = "image")]
        dzi_name: String,
        /// DZI tile size (pixels) when `--dzi` is set.
        #[arg(long, default_value_t = 512)]
        dzi_tile_size: u64,
        /// Disable the SSRF guard on `http(s)://` store specs — see `serve --help` for the full
        /// explanation. Also settable via `ZIV_ALLOW_INTERNAL_HOSTS`.
        #[arg(long)]
        allow_internal_hosts: bool,
    },
    /// Render one projection of an OME-Zarr image to a single PNG or JPEG file.
    ///
    /// The reason this earns its own command rather than "take a screenshot of `serve`": `--at`
    /// takes the exact same projection identifier the server understands (for example
    /// `@z=10,c=0,overlay=nuclei:distinct:0.6`), so one call bakes a label overlay — with its
    /// palette and opacity — into a flat image. No other tool does that in one step.
    Render {
        /// Store spec for the .ome.zarr image: a local filesystem path, or a remote URL
        /// (`s3://bucket/path`, `gs://bucket/path`, `az://container/path`,
        /// `http(s)://host/path`). See `serve --help` for remote credential env vars.
        src: String,
        /// Output file to write. The format is inferred from this extension unless --format
        /// overrides it.
        out: PathBuf,
        /// Projection identifier to render, in the exact grammar `serve`'s IIIF identifier
        /// segment understands: `default` for the image as its own metadata describes it, or a
        /// dynamic `@z=..,t=..,c=..,label=NAME[:PALETTE]` / `@...,overlay=NAME[:PALETTE[:OPACITY]]`
        /// selection.
        #[arg(long, default_value = "default")]
        at: String,
        /// Output size, in the same grammar as the server's IIIF `size` path segment: `max` (the
        /// region's native resolution), `w,h`, `w,` (width only), `,h` (height only), `!w,h`
        /// (fit inside, preserving aspect ratio), or `pct:N`.
        #[arg(long, default_value = "max")]
        size: String,
        /// Region to render, in the same grammar as the server's IIIF `region` path segment:
        /// `full`, `square`, `x,y,w,h`, or `pct:x,y,w,h`.
        #[arg(long, default_value = "full")]
        region: String,
        /// Output format: png or jpg. Inferred from <OUT>'s extension when omitted.
        #[arg(long)]
        format: Option<String>,
        /// JPEG encoder quality, 0-100 (default 85 if omitted). Applies to JPEG output only —
        /// given alongside PNG output, it is ignored, with a note rather than silently.
        #[arg(long)]
        quality: Option<u8>,
    },
    /// Print a shell completion script to stdout.
    ///
    /// Install by writing the output to your shell's completion directory, e.g.:
    /// `ziv completions bash > /etc/bash_completion.d/ziv` or
    /// `ziv completions zsh > "${fpath[1]}/_ziv"` (zsh) or
    /// `ziv completions fish > ~/.config/fish/completions/ziv.fish`.
    #[command(hide = true)]
    Completions {
        /// Shell to generate the completion script for.
        shell: Shell,
    },
    /// Print a roff man page to stdout.
    ///
    /// With no argument, prints the top-level `ziv(1)` page, which only summarizes each
    /// subcommand. Name a subcommand (e.g. `export`) to print that subcommand's own page
    /// (`ziv-export(1)`) instead, documenting its actual flags in full.
    ///
    /// Install by writing the output to a directory on `MANPATH`, e.g.:
    /// `ziv man > /usr/local/share/man/man1/ziv.1` or
    /// `ziv man export > /usr/local/share/man/man1/ziv-export.1`.
    #[command(hide = true)]
    Man {
        /// Subcommand to render a page for (e.g. `export`, `serve`). Omit for the top-level page.
        subcommand: Option<String>,
    },
}

/// Resolves the effective SSRF escape-hatch flag: the CLI flag OR'd with the
/// `ZIV_ALLOW_INTERNAL_HOSTS` env var (either one being set is enough to disable the guard),
/// mirroring how `--public-base-url`/`ZIV_PUBLIC_BASE_URL` both work. Takes the flag value as a
/// plain argument (rather than reading the env itself only inline at the call site) so this
/// resolution is a pure, directly testable function independent of process env state — mirrors
/// `check_require_auth`'s shape.
fn resolve_allow_internal_hosts(flag: bool) -> bool {
    flag || zarr_core::allow_internal_hosts_from_env()
}

/// Fail loud at startup, before binding, if the operator asked to require auth (`--require-auth`)
/// but left both secret env vars unset. Auth's actual values still come only from
/// `ZIV_AUTH_BEARER`/`ZIV_AUTH_HMAC_SECRET` (never argv); this check just decides whether
/// "neither is set" is a startup error or "serve open". Takes the already-resolved
/// `Option<AuthConfig>` (rather than reading the env itself) so it's a pure, directly testable
/// function independent of process env state.
fn check_require_auth(
    require_auth: bool,
    auth: &Option<server::AuthConfig>,
) -> Result<(), Box<dyn std::error::Error>> {
    if require_auth && auth.is_none() {
        return Err("--require-auth set but neither ZIV_AUTH_BEARER nor \
             ZIV_AUTH_HMAC_SECRET is set in the environment"
            .into());
    }
    Ok(())
}

/// Prints a top-level error and its full `source()` chain the way a CLI user should read it:
/// `Display` text, never `Debug`. `main`'s old `Result<(), Box<dyn Error>>` return type let the
/// Rust runtime print a returned `Err` itself, and it prints with `Debug` — so a refused export
/// showed `Error: UnsupportedPyramid { scale_factors: [1, 3, 9], levels: 3 }` instead of
/// `exporter::ExportError::UnsupportedPyramid`'s carefully written prose. Refusing an export is a
/// normal user-facing outcome now, so that Debug dump was a production defect, not a curiosity.
pub fn report_error(err: &dyn std::error::Error) {
    let mut shown = err.to_string();
    eprintln!("ziv: error: {shown}");
    let mut source = err.source();
    while let Some(cause) = source {
        // Many errors already interpolate their source into their own message (e.g.
        // `ExportError::Io`'s "io error at {path}: {source}"), so printing it again as a cause
        // would just repeat the same words on the next line.
        let text = cause.to_string();
        if !shown.contains(&text) {
            eprintln!("  caused by: {text}");
            shown = text;
        }
        source = cause.source();
    }
}

pub async fn run() -> Result<(), Box<dyn std::error::Error>> {
    server::init_tracing();
    let cli = Cli::parse();
    match cli.command {
        Command::Serve {
            paths,
            addr,
            require_auth,
            public_base_url,
            allow_internal_hosts,
        } => {
            check_require_auth(require_auth, &server::AuthConfig::from_env())?;
            let allow_internal_hosts = resolve_allow_internal_hosts(allow_internal_hosts);

            // The registry drives every open through `spawn_blocking` for the same reason this
            // used to: `ZarrImage::open`'s remote path runs async object_store reads under
            // `pollster::block_on`, which is safe from a plain OS thread or a multi-threaded tokio
            // runtime but would deadlock on a `current_thread` runtime's sole worker. See
            // `registry::ImageRegistry::get`.
            // Install the Prometheus recorder BEFORE the eager opens below, not when the router
            // is built. Otherwise every argv image opens while no recorder exists and its
            // `ziv_image_opens_total` / `ziv_image_open_seconds` samples are dropped, which is
            // exactly the timing an operator most wants on a slow remote store.
            server::metrics::recorder();

            let mut pairs = Vec::with_capacity(paths.len());
            for path in &paths {
                pairs.push((server::registry::name_from_path(path)?, path.clone()));
            }
            let registry = Arc::new(
                server::registry::ImageRegistry::builder()
                    .allow_internal_hosts(allow_internal_hosts)
                    .source(Box::new(server::registry::ExplicitSource::from_pairs(
                        pairs.clone(),
                    )?))
                    .build(),
            );
            // Eager, because someone who typed a path expects a broken one to fail now rather than
            // on the first tile. Every other source (a config file, a directory root) opens lazily.
            for (name, _) in &pairs {
                registry.get(name).await?;
            }
            let base_url = format!("http://{addr}");
            // Precedence: `--public-base-url` > `ZIV_PUBLIC_BASE_URL` env > (per-request
            // forwarded headers, applied inside `run_server`/`origin::resolve_base_url`) > the
            // bind address. Neither CLI flag nor env carries a secret, so (unlike auth) there's
            // no reason to forbid the flag form here.
            let public_base_url = public_base_url.or_else(|| {
                std::env::var("ZIV_PUBLIC_BASE_URL")
                    .ok()
                    .filter(|s| !s.is_empty())
            });
            server::run_server(&addr, registry, base_url, public_base_url).await?;
        }
        Command::Export {
            src,
            out_dir,
            id,
            quality,
            planes,
            labels,
            overlay_opacity,
            dzi: emit_dzi,
            dzi_name,
            dzi_tile_size,
            allow_internal_hosts,
        } => {
            let allow_internal_hosts = resolve_allow_internal_hosts(allow_internal_hosts);
            // The manifest's label. Derived exactly as `serve` names an image; an unnameable path
            // (it happens with odd remote URLs) falls back rather than failing the export.
            let name = server::registry::name_from_path(&src)
                .map(|n| n.as_str().to_string())
                .unwrap_or_else(|_| "image".to_string());
            // Same runtime-flavor-independent `spawn_blocking` reasoning as `serve`'s
            // `open` call above: `ZarrImage::open`'s remote path drives `block_on`
            // internally, so it must run on a dedicated blocking thread rather than
            // inline on an async task.
            let image = tokio::task::spawn_blocking(move || {
                ZarrImage::open_with_options(&src, allow_internal_hosts)
            })
            .await??;
            let engine = ZarrTileEngine::new(image);

            // The actual export (tile rendering + file IO) is synchronous, CPU/IO-bound
            // work with its own internal `rayon` parallelism — run it on a blocking
            // thread rather than the async executor, mirroring every other
            // zarr-touching call in this binary.
            let out_dir_for_export = out_dir.clone();
            let options = ExportOptions {
                id: id.unwrap_or_else(|| ".".to_string()),
                quality,
                planes,
                labels,
                overlay_opacity,
                name,
            };
            let exported = tokio::task::spawn_blocking(move || {
                export_with_progress(
                    &engine,
                    &out_dir_for_export,
                    &options,
                    &mut print_export_event,
                )
                .map(|summary| (summary, engine))
            })
            .await??;
            let (summary, engine) = exported;
            println!(
                "ziv export: wrote {} {}, {} {}, info.json and index.html to {}",
                summary.views,
                noun(summary.views, "view", "views"),
                summary.tiles,
                noun(summary.tiles, "tile file", "tile files"),
                out_dir.display()
            );
            if emit_dzi && (planes || labels) {
                eprintln!("ziv export: note: --dzi covers the default view only");
            }

            if emit_dzi {
                let out_dir_for_dzi = out_dir.clone();
                let dzi_name_for_export = dzi_name.clone();
                let dzi_count = tokio::task::spawn_blocking(move || {
                    dzi::export_dzi(
                        &engine,
                        &out_dir_for_dzi,
                        &dzi_name_for_export,
                        dzi_tile_size,
                        quality,
                    )
                })
                .await??;
                println!(
                    "ziv export --dzi: wrote {dzi_count} DZI tile files + {dzi_name}.dzi to {}",
                    out_dir.display()
                );
            }
        }
        Command::Render {
            src,
            out,
            at,
            size: size_arg,
            region: region_arg,
            format,
            quality,
        } => {
            let format = render::resolve_format(&out, format.as_deref())?;
            if quality.is_some() && format == iiif::Format::Png {
                eprintln!("ziv render: note: --quality is ignored for png output");
            }
            let jpeg_quality = quality.unwrap_or(85);
            // The same parsers `serve` uses for the region/size path segments and the projection
            // identifier. This is precisely what keeps `render` from ever accepting (or refusing)
            // something the server disagrees with.
            let region = iiif::parse_region(&region_arg)?;
            let size = iiif::parse_size(&size_arg)?;
            let id = iiif::parse_identifier(&at);

            // Opening a remote store's async reads must run on a dedicated blocking thread; see
            // `serve`'s own comment on `ImageRegistry::get` above.
            let image = tokio::task::spawn_blocking(move || ZarrImage::open(&src)).await??;
            let engine = ZarrTileEngine::new(image);

            let (bytes, out_w, out_h) = tokio::task::spawn_blocking(
                move || -> Result<(Vec<u8>, u32, u32), tiling::TileError> {
                    // Resolve the geometry BEFORE reading/compositing/encoding any pixels, so an
                    // over-budget request is refused instead of paying for the render it is about
                    // to throw away. This is the same problem a static export's level0 whole-image
                    // budget solves for `full/max`, reusing the very same constants.
                    let (out_w, out_h) = engine.output_size(region, size)?;
                    if !iiif::whole_image_within_budget(out_w as u64, out_h as u64) {
                        return Err(tiling::TileError::OutOfRange(render::budget_refusal(
                            out_w, out_h,
                        )));
                    }
                    let spec = tiling::RenderSpec {
                        region,
                        size,
                        rotation: 0,
                        quality: iiif::Quality::Default,
                        format,
                        jpeg_quality,
                    };
                    let bytes = engine.render(&id, &spec)?;
                    Ok((bytes, out_w, out_h))
                },
            )
            .await??;

            if let Some(parent) = out.parent() {
                if !parent.as_os_str().is_empty() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("creating {}: {e}", parent.display()))?;
                }
            }
            let byte_count = bytes.len();
            std::fs::write(&out, bytes).map_err(|e| format!("writing {}: {e}", out.display()))?;
            println!(
                "ziv render: wrote {out_w}x{out_h} {} ({byte_count} bytes) to {}",
                format.extension(),
                out.display()
            );
        }
        Command::Completions { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            clap_complete::generate(shell, &mut cmd, name, &mut std::io::stdout());
        }
        Command::Man { subcommand } => {
            let mut cmd = Cli::command();
            // `build()` is what makes clap fill in each subcommand's display name as
            // `{parent}-{child}` (e.g. `ziv-export`), which `find_subcommand` below then hands to
            // `clap_mangen::Man::new` — that display name becomes the page's title and, via
            // `Man::get_filename`, the `ziv-export.1` filename `generate_to` would pick.
            cmd.build();
            let target = match &subcommand {
                Some(name) => cmd
                    .find_subcommand(name)
                    .cloned()
                    .ok_or_else(|| format!("ziv man: no such subcommand: {name}"))?,
                None => cmd,
            };
            let man = clap_mangen::Man::new(target);
            man.render(&mut std::io::stdout())?;
        }
    }
    Ok(())
}

/// Picks `singular` for a count of exactly 1, `plural` otherwise. Views, tiles and tile files are
/// the only nouns this prints, so a plain singular-vs-plural pick is exact; nothing generic is
/// needed.
fn noun(count: usize, singular: &'static str, plural: &'static str) -> &'static str {
    if count == 1 {
        singular
    } else {
        plural
    }
}

/// Progress and warnings go to stderr, so stdout carries only the final summary line.
fn print_export_event(event: ExportEvent) {
    match event {
        ExportEvent::Planned {
            views,
            tiles_per_view,
        } => {
            let total = views * tiles_per_view;
            eprintln!(
                "ziv export: planning {views} {} × {tiles_per_view} {} = {total} {}",
                noun(views, "view", "views"),
                noun(tiles_per_view, "tile", "tiles"),
                noun(total, "tile", "tiles"),
            )
        }
        ExportEvent::Warning(warning) => eprintln!("ziv export: warning: {warning}"),
        ExportEvent::TreeWritten {
            index,
            total,
            folder,
            tiles,
        } => {
            eprintln!(
                "ziv export: [{index}/{total}] {folder} ({tiles} {})",
                noun(tiles, "tile", "tiles")
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn require_auth_false_never_errors_regardless_of_auth() {
        assert!(check_require_auth(false, &None).is_ok());
    }

    #[test]
    fn require_auth_true_with_no_auth_configured_errors() {
        assert!(check_require_auth(true, &None).is_err());
    }

    #[test]
    fn require_auth_true_with_auth_configured_is_ok() {
        let auth = server::AuthConfig {
            bearer: Some("tok".to_string()),
            hmac_secret: None,
        };
        assert!(check_require_auth(true, &Some(auth)).is_ok());
    }

    #[test]
    fn parses_serve_with_default_addr() {
        let cli = Cli::parse_from(["ziv", "serve", "/tmp/x.ome.zarr"]);
        match cli.command {
            Command::Serve {
                paths,
                addr,
                require_auth,
                public_base_url,
                allow_internal_hosts,
            } => {
                assert_eq!(paths, vec!["/tmp/x.ome.zarr".to_string()]);
                assert_eq!(addr, "127.0.0.1:3000");
                assert!(!require_auth);
                assert_eq!(public_base_url, None);
                assert!(!allow_internal_hosts);
            }
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn parses_serve_with_public_base_url_flag() {
        let cli = Cli::parse_from([
            "ziv",
            "serve",
            "/tmp/x.ome.zarr",
            "--public-base-url",
            "https://images.example.org",
        ]);
        match cli.command {
            Command::Serve {
                public_base_url, ..
            } => {
                assert_eq!(
                    public_base_url,
                    Some("https://images.example.org".to_string())
                );
            }
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn parses_serve_with_require_auth_flag() {
        let cli = Cli::parse_from(["ziv", "serve", "/tmp/x.ome.zarr", "--require-auth"]);
        match cli.command {
            Command::Serve { require_auth, .. } => {
                assert!(require_auth);
            }
            _ => panic!("expected Serve"),
        }
    }

    /// A remote URL store spec must pass through the CLI arg parser unchanged (no path
    /// normalization/validation at the clap layer — `ZarrImage::open`'s `parse_store_spec` is
    /// the single source of truth for interpreting it).
    #[test]
    fn parses_serve_with_remote_url() {
        let cli = Cli::parse_from(["ziv", "serve", "s3://my-bucket/images/sample.ome.zarr"]);
        match cli.command {
            Command::Serve { paths, .. } => {
                assert_eq!(
                    paths,
                    vec!["s3://my-bucket/images/sample.ome.zarr".to_string()]
                );
            }
            _ => panic!("expected Serve"),
        }
    }

    /// More than one image on one command line. Each becomes a catalogue entry named after its
    /// final path component; the server mounts them under `/i/{name}/`.
    #[test]
    fn parses_serve_with_several_paths() {
        let cli = Cli::parse_from(["ziv", "serve", "/tmp/a.ome.zarr", "s3://b/c.zarr"]);
        match cli.command {
            Command::Serve { paths, .. } => {
                assert_eq!(
                    paths,
                    vec!["/tmp/a.ome.zarr".to_string(), "s3://b/c.zarr".to_string()]
                );
            }
            _ => panic!("expected Serve"),
        }
    }

    /// `serve` with no path at all is a usage error, not a server bound to nothing.
    #[test]
    fn serve_requires_at_least_one_path() {
        assert!(Cli::try_parse_from(["ziv", "serve"]).is_err());
    }

    #[test]
    fn parses_export_with_defaults() {
        let cli = Cli::parse_from(["ziv", "export", "/tmp/x.ome.zarr", "/tmp/out"]);
        match cli.command {
            Command::Export {
                src,
                out_dir,
                id,
                quality,
                planes,
                labels,
                overlay_opacity,
                dzi,
                dzi_name,
                dzi_tile_size,
                allow_internal_hosts,
            } => {
                assert_eq!(src, "/tmp/x.ome.zarr");
                assert_eq!(out_dir, PathBuf::from("/tmp/out"));
                assert_eq!(id, None);
                assert_eq!(quality, 85);
                assert!(!planes);
                assert!(!labels);
                assert_eq!(overlay_opacity, 0.6);
                assert!(!dzi);
                assert_eq!(dzi_name, "image");
                assert_eq!(dzi_tile_size, 512);
                assert!(!allow_internal_hosts);
            }
            _ => panic!("expected Export"),
        }
    }

    #[test]
    fn parses_export_with_dzi_and_options() {
        let cli = Cli::parse_from([
            "ziv",
            "export",
            "s3://bucket/img.ome.zarr",
            "/tmp/out",
            "--id",
            "https://example.org/iiif/foo",
            "--quality",
            "90",
            "--dzi",
            "--dzi-name",
            "wsi",
            "--dzi-tile-size",
            "256",
        ]);
        match cli.command {
            Command::Export {
                src,
                out_dir,
                id,
                quality,
                planes,
                labels,
                overlay_opacity,
                dzi,
                dzi_name,
                dzi_tile_size,
                allow_internal_hosts,
            } => {
                assert_eq!(src, "s3://bucket/img.ome.zarr");
                assert_eq!(out_dir, PathBuf::from("/tmp/out"));
                assert_eq!(id, Some("https://example.org/iiif/foo".to_string()));
                assert_eq!(quality, 90);
                assert!(!planes);
                assert!(!labels);
                assert_eq!(overlay_opacity, 0.6);
                assert!(dzi);
                assert_eq!(dzi_name, "wsi");
                assert_eq!(dzi_tile_size, 256);
                assert!(!allow_internal_hosts);
            }
            _ => panic!("expected Export"),
        }
    }

    // --- --allow-internal-hosts flag (Deliverable A escape hatch) ---

    #[test]
    fn parses_serve_with_allow_internal_hosts_flag() {
        let cli = Cli::parse_from([
            "ziv",
            "serve",
            "http://169.254.169.254/x.ome.zarr",
            "--allow-internal-hosts",
        ]);
        match cli.command {
            Command::Serve {
                allow_internal_hosts,
                ..
            } => assert!(allow_internal_hosts),
            _ => panic!("expected Serve"),
        }
    }

    #[test]
    fn parses_export_with_allow_internal_hosts_flag() {
        let cli = Cli::parse_from([
            "ziv",
            "export",
            "http://169.254.169.254/x.ome.zarr",
            "/tmp/out",
            "--allow-internal-hosts",
        ]);
        match cli.command {
            Command::Export {
                allow_internal_hosts,
                ..
            } => assert!(allow_internal_hosts),
            _ => panic!("expected Export"),
        }
    }

    #[test]
    fn resolve_allow_internal_hosts_flag_true_is_always_true() {
        assert!(resolve_allow_internal_hosts(true));
    }

    #[test]
    fn resolve_allow_internal_hosts_flag_false_defers_to_env() {
        static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: exclusive access to this env var is guaranteed by ENV_LOCK for the duration
        // of mutation + the calls that read it.
        unsafe {
            std::env::remove_var("ZIV_ALLOW_INTERNAL_HOSTS");
        }
        assert!(!resolve_allow_internal_hosts(false));

        unsafe {
            std::env::set_var("ZIV_ALLOW_INTERNAL_HOSTS", "1");
        }
        let result = resolve_allow_internal_hosts(false);
        unsafe {
            std::env::remove_var("ZIV_ALLOW_INTERNAL_HOSTS");
        }
        assert!(result);
    }

    // --- completions / man subcommands ---

    #[test]
    fn parses_completions_bash() {
        let cli = Cli::parse_from(["ziv", "completions", "bash"]);
        match cli.command {
            Command::Completions { shell } => assert_eq!(shell, clap_complete::Shell::Bash),
            _ => panic!("expected Completions"),
        }
    }

    #[test]
    fn parses_man() {
        let cli = Cli::parse_from(["ziv", "man"]);
        match cli.command {
            Command::Man { subcommand } => assert_eq!(subcommand, None),
            _ => panic!("expected Man"),
        }
    }

    #[test]
    fn parses_man_with_subcommand() {
        let cli = Cli::parse_from(["ziv", "man", "export"]);
        match cli.command {
            Command::Man { subcommand } => assert_eq!(subcommand, Some("export".to_string())),
            _ => panic!("expected Man"),
        }
    }

    /// `Cli::command()` (clap's own internal consistency check) must not panic — a cheap,
    /// standard regression guard against malformed `#[arg(...)]`/`#[command(...)]` attributes
    /// that clap only catches when the `Command` is actually built (e.g. by `--help`, or here).
    #[test]
    fn cli_command_builds_without_panicking() {
        Cli::command().debug_assert();
    }

    #[test]
    fn export_parses_the_view_flags() {
        let cli = Cli::try_parse_from([
            "ziv",
            "export",
            "in.ome.zarr",
            "out",
            "--planes",
            "--labels",
            "--overlay-opacity",
            "0.4",
        ])
        .unwrap();
        let Command::Export {
            planes,
            labels,
            overlay_opacity,
            ..
        } = cli.command
        else {
            panic!("expected export");
        };
        assert!(planes && labels);
        assert_eq!(overlay_opacity, 0.4);
    }

    // --- render subcommand parsing ---

    #[test]
    fn parses_render_with_defaults() {
        let cli = Cli::parse_from(["ziv", "render", "/tmp/x.ome.zarr", "/tmp/out.png"]);
        match cli.command {
            Command::Render {
                src,
                out,
                at,
                size,
                region,
                format,
                quality,
            } => {
                assert_eq!(src, "/tmp/x.ome.zarr");
                assert_eq!(out, PathBuf::from("/tmp/out.png"));
                assert_eq!(at, "default");
                assert_eq!(size, "max");
                assert_eq!(region, "full");
                assert_eq!(format, None);
                assert_eq!(quality, None);
            }
            _ => panic!("expected Render"),
        }
    }

    #[test]
    fn parses_render_with_all_flags() {
        let cli = Cli::parse_from([
            "ziv",
            "render",
            "s3://bucket/img.ome.zarr",
            "out.jpg",
            "--at",
            "@z=10,c=0,overlay=nuclei:distinct:0.6",
            "--size",
            "1024,",
            "--region",
            "0,0,500,500",
            "--format",
            "jpg",
            "--quality",
            "90",
        ]);
        match cli.command {
            Command::Render {
                src,
                out,
                at,
                size,
                region,
                format,
                quality,
            } => {
                assert_eq!(src, "s3://bucket/img.ome.zarr");
                assert_eq!(out, PathBuf::from("out.jpg"));
                assert_eq!(at, "@z=10,c=0,overlay=nuclei:distinct:0.6");
                assert_eq!(size, "1024,");
                assert_eq!(region, "0,0,500,500");
                assert_eq!(format, Some("jpg".to_string()));
                assert_eq!(quality, Some(90));
            }
            _ => panic!("expected Render"),
        }
    }
}
