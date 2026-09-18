//! Writes the built-in viewer into an export, so the folder opens and works with no network
//! access beyond its own files: the whole point of a static export is zero backend, which
//! includes not depending on a CDN for the viewer.
//!
//! The viewer is the SAME `crates/viewer-assets/assets/viewer/` tree `ziv serve` embeds, via the
//! shared `viewer_assets::ViewerAssets`. One page, one `viewer.js`: the export rewrites the
//! page's mode marker to `static`, writes the page at the export root, and everything else under
//! `viewer/`, which is where the page's loader looks in static mode.

use std::fs;
use std::path::Path;
use viewer_assets::ViewerAssets;

use crate::writer::ExportError;

/// The marker `crates/viewer-assets/assets/viewer/index.html` carries as served.
pub const SERVER_MODE_MARKER: &str = r#"<meta name="ziv-mode" content="server">"#;
/// What an export turns it into.
pub const STATIC_MODE_MARKER: &str = r#"<meta name="ziv-mode" content="static">"#;

/// The shared viewer page with its mode switched to static.
pub fn static_index_html() -> String {
    let page = ViewerAssets::get("index.html").expect("assets/viewer/index.html is embedded");
    String::from_utf8_lossy(&page.data).replacen(SERVER_MODE_MARKER, STATIC_MODE_MARKER, 1)
}

/// Write `index.html` at `out_dir`'s root and every other viewer asset under `out_dir/viewer/`.
pub fn write_viewer(out_dir: &Path) -> Result<(), ExportError> {
    write(&out_dir.join("index.html"), static_index_html().as_bytes())?;
    for path in ViewerAssets::iter() {
        if path == "index.html" {
            continue;
        }
        let contents = ViewerAssets::get(&path).expect("path came from ViewerAssets::iter()");
        write(&out_dir.join("viewer").join(path.as_ref()), &contents.data)?;
    }
    Ok(())
}

fn write(path: &Path, bytes: &[u8]) -> Result<(), ExportError> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| io_err(parent, e))?;
    }
    fs::write(path, bytes).map_err(|e| io_err(path, e))
}

fn io_err(path: &Path, source: std::io::Error) -> ExportError {
    ExportError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exported_page_is_the_shared_page_in_static_mode() {
        let page = static_index_html();
        assert_eq!(page.matches(STATIC_MODE_MARKER).count(), 1);
        assert_eq!(page.matches(SERVER_MODE_MARKER).count(), 0);
        assert!(page.contains("viewer.js"));
    }

    #[test]
    fn writes_the_page_at_the_root_and_its_assets_under_viewer() {
        let dir = tempfile::tempdir().unwrap();
        write_viewer(dir.path()).unwrap();
        for file in [
            "index.html",
            "viewer/viewer.js",
            "viewer/viewer.css",
            "viewer/openseadragon/openseadragon.min.js",
            "viewer/nav/nav.js",
        ] {
            assert!(dir.path().join(file).exists(), "missing {file}");
        }
        assert!(
            !dir.path().join("viewer/index.html").exists(),
            "the page is written once"
        );
        assert!(!dir.path().join("osd").exists() && !dir.path().join("nav").exists());
    }

    /// BSD-3-Clause (OpenSeadragon), ISC (Lucide) and MIT (Feather, which most icons derive from)
    /// all require their notices to travel with every copy, and every export is a copy.
    #[test]
    fn exported_tree_carries_every_licence() {
        let dir = tempfile::tempdir().unwrap();
        write_viewer(dir.path()).unwrap();
        let osd = fs::read_to_string(dir.path().join("viewer/openseadragon/LICENSE.txt")).unwrap();
        assert!(osd.to_lowercase().contains("redistribution"));
        let lucide = fs::read_to_string(dir.path().join("viewer/nav/LICENSE-lucide.txt")).unwrap();
        assert!(lucide.contains("ISC License") && lucide.contains("Cole Bemis"));
    }
}
