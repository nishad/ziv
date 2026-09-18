use serde_json::{json, Value};

#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    pub id: String,
    pub width: u64,
    pub height: u64,
    pub tile_size: u64,
    pub scale_factors: Vec<u64>,
    pub sizes: Vec<(u64, u64)>,
}

impl ImageInfo {
    /// Serialize to a IIIF Image API 3.0 info.json. `level` is 0 (static) or 2 (dynamic).
    pub fn to_info_json(&self, level: u8) -> Value {
        json!({
            "@context": "http://iiif.io/api/image/3/context.json",
            "id": self.id,
            "type": "ImageService3",
            "protocol": "http://iiif.io/api/image",
            "profile": format!("level{}", level),
            "width": self.width,
            "height": self.height,
            "tiles": [{ "width": self.tile_size, "scaleFactors": self.scale_factors }],
            "sizes": self.sizes.iter().map(|(w, h)| json!({"width": w, "height": h})).collect::<Vec<_>>(),
        })
    }

    /// Serialize to a IIIF Image API 3.0 info.json for a STATIC (`export`) Level-0 tile tree.
    ///
    /// Adds `profile: "level0"` and `extraFeatures: ["sizeByWh"]` on top of the level-2 shape.
    /// `sizeByWh` is what makes OpenSeadragon's `canBeTiled()` pass for a level0 profile (see
    /// `iiiftilesource.js`), so it requests region/size tiles rather than falling back to
    /// whole-image-only legacy behaviour.
    ///
    /// Also narrows what the document promises to what an export actually writes: when the
    /// bound (`crate::level0::Level0Sizes::within_budget`) holds, `maxWidth` and `maxHeight`
    /// cap whole-image requests, and `sizes` lists only sizes within that bound. Above the
    /// budget an export writes no whole images at all (see `MAX_WHOLE_IMAGE_PIXELS`), so
    /// declaring a bound or a trimmed `sizes` would advertise a size whose file does not exist;
    /// instead `sizes` stays untrimmed and no bound is declared. Full resolution stays reachable
    /// through `tiles` either way. See [`crate::level0`] for why exactly one entry may be
    /// dropped and no more.
    ///
    /// # Panics
    /// Panics if OpenSeadragon cannot pin this pyramid's level sizes from any `sizes` array
    /// (`crate::level0::level0_sizes` returns `None`). Deliberately a hard failure: such a tree
    /// would send the exported viewer after files the export does not contain, and a blank viewer
    /// is a worse outcome than a refused export.
    pub fn to_info_json_level0(&self) -> Value {
        let plan = crate::level0::level0_sizes(self).unwrap_or_else(|| {
            panic!(
                "OpenSeadragon cannot pin the level sizes of this pyramid: scaleFactors {:?} with \
                 {} levels. A level0 tree built from it would request files it does not contain.",
                self.scale_factors,
                self.sizes.len()
            )
        });
        let mut v = self.to_info_json(0);
        v["extraFeatures"] = json!(["sizeByWh"]);
        if plan.within_budget() {
            v["sizes"] = json!(plan
                .sizes
                .iter()
                .map(|(w, h)| json!({"width": w, "height": h}))
                .collect::<Vec<_>>());
            v["maxWidth"] = json!(plan.max_width);
            v["maxHeight"] = json!(plan.max_height);
        }
        v
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn level0_declares_a_bound_and_advertises_only_what_is_written() {
        let info = ImageInfo {
            id: ".".into(),
            width: 1024,
            height: 1024,
            tile_size: 512,
            scale_factors: vec![1, 2],
            sizes: vec![(1024, 1024), (512, 512)],
        };
        let v = info.to_info_json_level0();
        assert_eq!(v["maxWidth"], 512);
        assert_eq!(v["maxHeight"], 512);
        assert_eq!(
            v["sizes"],
            serde_json::json!([{"width": 512, "height": 512}])
        );
        // The tiled pyramid is untouched: full resolution is still reachable, tiled.
        assert_eq!(v["width"], 1024);
        assert_eq!(v["tiles"][0]["scaleFactors"], serde_json::json!([1, 2]));
    }

    #[test]
    fn level2_still_advertises_every_size() {
        let info = ImageInfo {
            id: ".".into(),
            width: 1024,
            height: 1024,
            tile_size: 512,
            scale_factors: vec![1, 2],
            sizes: vec![(1024, 1024), (512, 512)],
        };
        let v = info.to_info_json(2);
        assert_eq!(v["sizes"].as_array().unwrap().len(), 2);
        assert!(v.get("maxWidth").is_none());
    }

    #[test]
    #[should_panic(expected = "OpenSeadragon cannot pin")]
    fn level0_refuses_a_pyramid_openseadragon_cannot_pin() {
        let info = ImageInfo {
            id: ".".into(),
            width: 900,
            height: 900,
            tile_size: 512,
            scale_factors: vec![1, 3, 9],
            sizes: vec![(900, 900), (300, 300), (100, 100)],
        };
        let _ = info.to_info_json_level0();
    }

    #[test]
    fn level0_above_budget_advertises_no_bound_and_the_untrimmed_sizes() {
        let info = ImageInfo {
            id: ".".into(),
            width: 100_000,
            height: 100_000,
            tile_size: 512,
            scale_factors: vec![1],
            sizes: vec![(100_000, 100_000)],
        };
        let v = info.to_info_json_level0();
        assert!(v.get("maxWidth").is_none());
        assert!(v.get("maxHeight").is_none());
        assert_eq!(
            v["sizes"],
            serde_json::json!([{"width": 100_000, "height": 100_000}])
        );
    }

    #[test]
    fn emits_level2_info_json() {
        let info = ImageInfo {
            id: "https://host/iiif/img/default".into(),
            width: 1024,
            height: 768,
            tile_size: 512,
            scale_factors: vec![1, 2, 4],
            sizes: vec![(1024, 768), (128, 96)],
        };
        let v = info.to_info_json(2);
        assert_eq!(v["profile"], "level2");
        assert_eq!(v["type"], "ImageService3");
        assert_eq!(v["width"], 1024);
        assert_eq!(v["tiles"][0]["width"], 512);
        assert_eq!(v["tiles"][0]["scaleFactors"][2], 4);
        assert_eq!(v["sizes"][1]["width"], 128);
        assert_eq!(v["@context"], "http://iiif.io/api/image/3/context.json");
    }

    fn level0_info() -> ImageInfo {
        ImageInfo {
            id: "https://host/export/root".into(),
            width: 1024,
            height: 768,
            tile_size: 256,
            scale_factors: vec![1, 2, 4],
            sizes: vec![(1024, 768), (512, 384), (256, 192)],
        }
    }

    /// info.json shape: profile level0, extraFeatures has sizeByWh, @context v3. This pyramid's
    /// full-resolution entry is dropped (OSD reconstructs it, see `crate::level0`), so `sizes`
    /// has one fewer entry than `scale_factors`, not the same count — the old 1:1 proxy this
    /// design replaces would have rejected exactly this, honestly trimmed, tree.
    #[test]
    fn emits_level0_info_json_with_size_by_wh() {
        let v = level0_info().to_info_json_level0();
        assert_eq!(v["profile"], "level0");
        assert_eq!(v["extraFeatures"], json!(["sizeByWh"]));
        assert_eq!(v["@context"], "http://iiif.io/api/image/3/context.json");
        assert_eq!(v["type"], "ImageService3");
        assert_eq!(v["sizes"].as_array().unwrap().len(), 2);
    }

    /// Existing level2 info.json emission must keep working unchanged after adding the
    /// level0 variant.
    #[test]
    fn level2_info_json_still_has_no_profile0_or_extra_features() {
        let v = level0_info().to_info_json(2);
        assert_eq!(v["profile"], "level2");
        assert!(v.get("extraFeatures").is_none());
    }
}
