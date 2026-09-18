# Print an optspec for argparse to handle cmd's options that are independent of any subcommand.
function __fish_ziv_global_optspecs
    string join \n h/help V/version
end

function __fish_ziv_needs_command
    # Figure out if the current invocation already has a command.
    set -l cmd (commandline -opc)
    set -e cmd[1]
    argparse -s (__fish_ziv_global_optspecs) -- $cmd 2>/dev/null
    or return
    if set -q argv[1]
        # Also print the command, so this can be used to figure out what it is.
        echo $argv[1]
        return 1
    end
    return 0
end

function __fish_ziv_using_subcommand
    set -l cmd (__fish_ziv_needs_command)
    test -z "$cmd"
    and return 1
    contains -- $cmd[1] $argv
end

complete -c ziv -n "__fish_ziv_needs_command" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c ziv -n "__fish_ziv_needs_command" -s V -l version -d 'Print version'
complete -c ziv -n "__fish_ziv_needs_command" -f -a "serve" -d 'Serve one or more OME-Zarr images as a live IIIF Image API 3.0 server'
complete -c ziv -n "__fish_ziv_needs_command" -f -a "export" -d 'Export an OME-Zarr image as a self-contained static IIIF Image API 3.0 Level-0 tile tree: every tile file OpenSeadragon v3 will request, a level0 info.json that pins the finite request space, and an embedded viewer. Drop the output directory on any static host (S3/GitHub Pages/CDN) for a live zoomable image with zero backend'
complete -c ziv -n "__fish_ziv_needs_command" -f -a "render" -d 'Render one projection of an OME-Zarr image to a single PNG or JPEG file'
complete -c ziv -n "__fish_ziv_needs_command" -f -a "completions" -d 'Print a shell completion script to stdout'
complete -c ziv -n "__fish_ziv_needs_command" -f -a "man" -d 'Print a roff man page to stdout'
complete -c ziv -n "__fish_ziv_needs_command" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
complete -c ziv -n "__fish_ziv_using_subcommand serve" -l addr -d 'Address to bind' -r
complete -c ziv -n "__fish_ziv_using_subcommand serve" -l public-base-url -d 'Public origin (e.g. `https://images.example.org`) to embed in the IIIF `id` (and any HMAC-signed URLs) instead of the bind address. Set this when ziv sits behind a TLS-terminating reverse proxy so `info.json`\'s `id` reflects the address clients actually use, not `--addr`. Also settable via `ZIV_PUBLIC_BASE_URL` (this flag wins if both are set). If neither is set, ziv falls back to honoring per-request `X-Forwarded-Proto`/`X-Forwarded-Host` headers when present, then finally the bind address — see `server::origin` for the full precedence and trust considerations of honoring forwarded headers by default' -r
complete -c ziv -n "__fish_ziv_using_subcommand serve" -l require-auth -d 'Fail startup unless auth is configured. Auth itself is always read from `ZIV_AUTH_BEARER`/`ZIV_AUTH_HMAC_SECRET` env vars (never a CLI flag — see those vars\' docs); this flag only turns "neither var set" into a startup error instead of quietly serving unauthenticated, for deployments that want to fail loud on a missing secret rather than accidentally serve in the open'
complete -c ziv -n "__fish_ziv_using_subcommand serve" -l allow-internal-hosts -d 'Disable the SSRF guard on `http(s)://` store specs, permitting loopback/link-local (including the cloud metadata endpoint 169.254.169.254)/private-network hosts. OFF by default (secure by default) — only set this for a legitimate internal deployment (e.g. an in-VPC MinIO reachable at a private IP). Also settable via `ZIV_ALLOW_INTERNAL_HOSTS` (either one being set enables it). See `zarr_core::store::ssrf` for the exact blocked ranges'
complete -c ziv -n "__fish_ziv_using_subcommand serve" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c ziv -n "__fish_ziv_using_subcommand export" -l id -d 'The IIIF id of the root tree. Defaults to . (relative to the export root). With --planes or --labels it is also the base of every other tree\'s id: pass the URL the export will be hosted at for absolute, standard ids. A trailing slash is trimmed (a IIIF id must not carry one), and an empty string is rewritten to "."' -r
complete -c ziv -n "__fish_ziv_using_subcommand export" -l quality -d 'JPEG quality (1-100) used for every rendered tile' -r
complete -c ziv -n "__fish_ziv_using_subcommand export" -l overlay-opacity -d 'Opacity of exported label overlays, 0 to 1' -r
complete -c ziv -n "__fish_ziv_using_subcommand export" -l dzi-name -d 'Base name for the DZI descriptor/tile directory when `--dzi` is set' -r
complete -c ziv -n "__fish_ziv_using_subcommand export" -l dzi-tile-size -d 'DZI tile size (pixels) when `--dzi` is set' -r
complete -c ziv -n "__fish_ziv_using_subcommand export" -l planes -d 'Also export every z-plane, each as its own Level-0 tile tree under `planes/{z}/`, at the default timepoint and channels. The exported viewer gets a z slider'
complete -c ziv -n "__fish_ziv_using_subcommand export" -l labels -d 'Also export an overlay of every label image on each exported plane, under `planes/{z}/labels/{i}/`, in distinct colours. The exported viewer gets a label picker'
complete -c ziv -n "__fish_ziv_using_subcommand export" -l dzi -d 'Also emit a DeepZoom (DZI) tree alongside the IIIF export, sharing the same raster: `{name}.dzi` + `{name}_files/{level}/{col}_{row}.jpg`'
complete -c ziv -n "__fish_ziv_using_subcommand export" -l allow-internal-hosts -d 'Disable the SSRF guard on `http(s)://` store specs — see `serve --help` for the full explanation. Also settable via `ZIV_ALLOW_INTERNAL_HOSTS`'
complete -c ziv -n "__fish_ziv_using_subcommand export" -s h -l help -d 'Print help'
complete -c ziv -n "__fish_ziv_using_subcommand render" -l at -d 'Projection identifier to render, in the exact grammar `serve`\'s IIIF identifier segment understands: `default` for the image as its own metadata describes it, or a dynamic `@z=..,t=..,c=..,label=NAME[:PALETTE]` / `@...,overlay=NAME[:PALETTE[:OPACITY]]` selection' -r
complete -c ziv -n "__fish_ziv_using_subcommand render" -l size -d 'Output size, in the same grammar as the server\'s IIIF `size` path segment: `max` (the region\'s native resolution), `w,h`, `w,` (width only), `,h` (height only), `!w,h` (fit inside, preserving aspect ratio), or `pct:N`' -r
complete -c ziv -n "__fish_ziv_using_subcommand render" -l region -d 'Region to render, in the same grammar as the server\'s IIIF `region` path segment: `full`, `square`, `x,y,w,h`, or `pct:x,y,w,h`' -r
complete -c ziv -n "__fish_ziv_using_subcommand render" -l format -d 'Output format: png or jpg. Inferred from <OUT>\'s extension when omitted' -r
complete -c ziv -n "__fish_ziv_using_subcommand render" -l quality -d 'JPEG encoder quality, 0-100 (default 85 if omitted). Applies to JPEG output only — given alongside PNG output, it is ignored, with a note rather than silently' -r
complete -c ziv -n "__fish_ziv_using_subcommand render" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c ziv -n "__fish_ziv_using_subcommand completions" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c ziv -n "__fish_ziv_using_subcommand man" -s h -l help -d 'Print help (see more with \'--help\')'
complete -c ziv -n "__fish_ziv_using_subcommand help; and not __fish_seen_subcommand_from serve export render completions man help" -f -a "serve" -d 'Serve one or more OME-Zarr images as a live IIIF Image API 3.0 server'
complete -c ziv -n "__fish_ziv_using_subcommand help; and not __fish_seen_subcommand_from serve export render completions man help" -f -a "export" -d 'Export an OME-Zarr image as a self-contained static IIIF Image API 3.0 Level-0 tile tree: every tile file OpenSeadragon v3 will request, a level0 info.json that pins the finite request space, and an embedded viewer. Drop the output directory on any static host (S3/GitHub Pages/CDN) for a live zoomable image with zero backend'
complete -c ziv -n "__fish_ziv_using_subcommand help; and not __fish_seen_subcommand_from serve export render completions man help" -f -a "render" -d 'Render one projection of an OME-Zarr image to a single PNG or JPEG file'
complete -c ziv -n "__fish_ziv_using_subcommand help; and not __fish_seen_subcommand_from serve export render completions man help" -f -a "completions" -d 'Print a shell completion script to stdout'
complete -c ziv -n "__fish_ziv_using_subcommand help; and not __fish_seen_subcommand_from serve export render completions man help" -f -a "man" -d 'Print a roff man page to stdout'
complete -c ziv -n "__fish_ziv_using_subcommand help; and not __fish_seen_subcommand_from serve export render completions man help" -f -a "help" -d 'Print this message or the help of the given subcommand(s)'
