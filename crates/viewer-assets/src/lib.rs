//! The embedded OpenSeadragon viewer: the vendored `assets/viewer/` tree (`index.html`, the
//! `viewer.js` and `viewer.css` it loads, the nav buttons and the OpenSeadragon build), embedded
//! via `rust-embed` exactly once and shared by both consumers.
//!
//! `ziv-server` serves this tree over HTTP (`/viewer/` and `/viewer/{*file}`, see its own
//! `viewer.rs`) and `ziv-exporter` writes it into a static export (rewriting the mode marker on
//! `index.html` to switch it from live to static, see its own `viewer.rs`). Both used to derive
//! their own `#[derive(Embed)]` struct over a shared path pointing above their package root
//! (`../../assets/viewer/`), which meant the ~500 KB tree was embedded twice and, since
//! `cargo package` only ever includes files under a package's own root, could never survive
//! being published to crates.io at all. Owning the embed here, under this crate's own root,
//! fixes both problems at once.

use rust_embed::Embed;

/// The vendored `assets/viewer/` tree: `index.html`, `viewer.js`, `viewer.css`, the nav
/// buttons, and the vendored OpenSeadragon build (plus its BSD-3-Clause `LICENSE.txt`).
#[derive(Embed)]
#[folder = "assets/viewer/"]
pub struct ViewerAssets;

#[cfg(test)]
mod tests {
    use super::*;

    /// The exporter turns this marker into static mode (see `exporter::viewer`). Exactly one, so
    /// the substitution can neither miss nor hit twice.
    #[test]
    fn the_embedded_page_carries_the_server_mode_marker_exactly_once() {
        let page = ViewerAssets::get("index.html").unwrap();
        let page = std::str::from_utf8(&page.data).unwrap();
        assert_eq!(
            page.matches(r#"<meta name="ziv-mode" content="server">"#)
                .count(),
            1
        );
    }

    /// ISC (Lucide) and MIT (Feather, which most of the icons derive from) both require the
    /// notice to travel with every copy of the embedded nav icons, in the binary and in every
    /// export.
    #[test]
    fn carries_the_lucide_license_text() {
        let bytes = ViewerAssets::get("nav/LICENSE-lucide.txt").unwrap();
        let text = String::from_utf8_lossy(&bytes.data);
        assert!(text.contains("ISC License") && text.contains("Lucide"));
        assert!(text.contains("MIT License") && text.contains("Cole Bemis"));
    }

    /// BSD-3-Clause (OpenSeadragon's license) requires reproducing the copyright notice and
    /// license text in redistributions. `ViewerAssets` embeds the whole `assets/viewer/` tree,
    /// so `LICENSE.txt` vendored alongside `openseadragon.min.js` is embedded (and thus
    /// reachable by both consumers) exactly like any other viewer asset.
    #[test]
    fn carries_the_vendored_openseadragon_license_text() {
        let bytes = ViewerAssets::get("openseadragon/LICENSE.txt").unwrap();
        let text = String::from_utf8_lossy(&bytes.data);
        assert!(
            text.contains("OpenSeadragon") || text.contains("CodePlex"),
            "embedded LICENSE.txt does not look like the OpenSeadragon BSD-3-Clause license"
        );
        assert!(
            text.to_lowercase().contains("redistribution"),
            "embedded LICENSE.txt is missing BSD-3-Clause redistribution terms"
        );
    }

    /// A live server and a static export must both be able to work offline: the one URL allowed
    /// is the SVG namespace, an identifier the browser never fetches.
    #[test]
    fn nothing_the_viewer_ships_reaches_the_network() {
        for file in ["index.html", "viewer.js", "viewer.css", "nav/nav.js"] {
            let bytes = ViewerAssets::get(file).unwrap_or_else(|| panic!("{file} embedded"));
            let text =
                String::from_utf8_lossy(&bytes.data).replace("http://www.w3.org/2000/svg", "");
            assert!(
                !text.contains("http://") && !text.contains("https://"),
                "{file}"
            );
        }
    }
}
