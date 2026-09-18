#[derive(Debug, Clone, PartialEq)]
pub enum ProjectionId {
    Default,
    Named(String),
    Dynamic(DynamicProj),
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct DynamicProj {
    pub z: Option<u64>,
    pub t: Option<u64>,
    pub channels: Vec<DynChannel>,
    /// A label image to render instead of the intensity channels, when the identifier names one.
    pub label: Option<DynLabel>,
}

/// A label image to draw, from either `label=NAME[:PALETTE]` or
/// `overlay=NAME[:PALETTE[:OPACITY]]`.
///
/// Two spellings rather than one with a mode flag, because they are two different pictures and a
/// URL should say which one it is without the reader having to know a defaulting rule:
/// `label=` draws the mask alone on black, `overlay=` composites it over the intensity render.
///
/// `NAME` is the group name under `labels/`. It cannot contain `,` or `:`, the two characters
/// this grammar spends on structure; a label whose name uses either is not addressable, which is
/// an acceptable limit for a name that is a zarr group name in practice.
#[derive(Debug, Clone, PartialEq)]
pub struct DynLabel {
    pub name: String,
    /// Uninterpreted here, exactly like `DynChannel::color`: the renderer owns the vocabulary.
    pub palette: Option<String>,
    /// True for `overlay=`: composite over the channels instead of replacing them.
    pub overlay: bool,
    /// The mask's alpha over the base, from `overlay=`'s third field. `None` means the renderer's
    /// own default. Unvalidated here — the parser never fails, so range checking is the
    /// renderer's job, the same as an unknown palette name.
    pub opacity: Option<f64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct DynChannel {
    pub index: usize,
    pub color: Option<String>,
    pub window: Option<(f64, f64)>,
}

/// Parses ziv's IIIF identifier segment into a projection.
///
/// `default` is the whole-image default rendering; a leading `@` introduces a DYNAMIC projection
/// selecting a plane and/or channels (`z=`, `t=`, and repeatable `c=INDEX[:COLOR[:LO-HI]]`);
/// anything else names a pre-configured projection.
///
/// ```
/// use iiif::{parse_identifier, ProjectionId};
///
/// assert_eq!(parse_identifier("default"), ProjectionId::Default);
/// assert_eq!(parse_identifier("dapi-overlay"), ProjectionId::Named("dapi-overlay".into()));
///
/// let ProjectionId::Dynamic(proj) = parse_identifier("@z=14,c=1:cyan:100-4000") else {
///     panic!("expected a dynamic projection");
/// };
/// assert_eq!(proj.z, Some(14));
/// assert_eq!(proj.channels[0].index, 1);
/// assert_eq!(proj.channels[0].color.as_deref(), Some("cyan"));
/// assert_eq!(proj.channels[0].window, Some((100.0, 4000.0)));
/// ```
///
/// This runs on an untrusted HTTP path segment, so it never fails and never panics: an
/// unparseable component is simply not applied.
///
/// ```
/// use iiif::{parse_identifier, ProjectionId};
///
/// let ProjectionId::Dynamic(proj) = parse_identifier("@z=not-a-number") else {
///     panic!("expected a dynamic projection");
/// };
/// assert_eq!(proj.z, None);
/// ```
pub fn parse_identifier(id: &str) -> ProjectionId {
    if id == "default" {
        return ProjectionId::Default;
    }
    if let Some(spec) = id.strip_prefix('@') {
        let mut proj = DynamicProj::default();
        for part in spec.split(',') {
            if let Some(v) = part.strip_prefix("z=") {
                proj.z = v.parse().ok();
            } else if let Some(v) = part.strip_prefix("t=") {
                proj.t = v.parse().ok();
            } else if let Some(v) = part.strip_prefix("label=") {
                // label=NAME[:PALETTE] — the mask alone. Split into at most two, so a stray third
                // field lands in the palette and is rejected downstream rather than dropped.
                let mut it = v.splitn(2, ':');
                let name = it.next().unwrap_or_default();
                if !name.is_empty() {
                    proj.label = Some(DynLabel {
                        name: name.to_string(),
                        palette: it.next().map(String::from),
                        overlay: false,
                        opacity: None,
                    });
                }
            } else if let Some(v) = part.strip_prefix("overlay=") {
                // overlay=NAME[:PALETTE[:OPACITY]] — the mask over the intensity render.
                let mut it = v.splitn(3, ':');
                let name = it.next().unwrap_or_default();
                if !name.is_empty() {
                    proj.label = Some(DynLabel {
                        name: name.to_string(),
                        palette: it.next().map(String::from),
                        overlay: true,
                        opacity: it.next().and_then(|o| o.parse().ok()),
                    });
                }
            } else if let Some(v) = part.strip_prefix("c=") {
                // c=INDEX[:COLOR[:LO-HI]]
                let mut it = v.split(':');
                if let Some(idx) = it.next().and_then(|s| s.parse::<usize>().ok()) {
                    let color = it.next().map(String::from);
                    let window = it
                        .next()
                        .and_then(|w| w.split_once('-'))
                        .and_then(|(lo, hi)| {
                            Some((lo.parse::<f64>().ok()?, hi.parse::<f64>().ok()?))
                        });
                    proj.channels.push(DynChannel {
                        index: idx,
                        color,
                        window,
                    });
                }
            }
        }
        return ProjectionId::Dynamic(proj);
    }
    ProjectionId::Named(id.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default() {
        assert_eq!(parse_identifier("default"), ProjectionId::Default);
    }

    #[test]
    fn parses_named() {
        assert_eq!(
            parse_identifier("z14-cDAPI"),
            ProjectionId::Named("z14-cDAPI".into())
        );
    }

    #[test]
    fn parses_a_label_selection() {
        let ProjectionId::Dynamic(d) = parse_identifier("@label=nuclei,z=3") else {
            panic!("expected Dynamic");
        };
        assert_eq!(
            d.label,
            Some(DynLabel {
                name: "nuclei".into(),
                palette: None,
                overlay: false,
                opacity: None,
            })
        );
        assert_eq!(d.z, Some(3));
    }

    #[test]
    fn parses_a_label_palette() {
        let ProjectionId::Dynamic(d) = parse_identifier("@label=0:table") else {
            panic!("expected Dynamic");
        };
        assert_eq!(d.label.unwrap().palette.as_deref(), Some("table"));
    }

    /// An empty name selects nothing rather than a label called "". Consistent with the rest of
    /// this parser: an unusable component is not applied, and never fails the request.
    #[test]
    fn an_empty_label_name_selects_no_label() {
        for spec in ["@label=", "@overlay="] {
            let ProjectionId::Dynamic(d) = parse_identifier(spec) else {
                panic!("expected Dynamic");
            };
            assert!(d.label.is_none(), "{spec}");
        }
    }

    #[test]
    fn parses_an_overlay_with_a_palette_and_opacity() {
        let ProjectionId::Dynamic(d) = parse_identifier("@c=1,overlay=nuclei:table:0.5") else {
            panic!("expected Dynamic");
        };
        assert_eq!(
            d.label,
            Some(DynLabel {
                name: "nuclei".into(),
                palette: Some("table".into()),
                overlay: true,
                opacity: Some(0.5),
            })
        );
        // The channels still apply: an overlay draws the mask ON the intensity render.
        assert_eq!(d.channels[0].index, 1);
    }

    /// `overlay=` without an opacity leaves it to the renderer rather than guessing here.
    #[test]
    fn an_overlay_without_an_opacity_leaves_it_unset() {
        let ProjectionId::Dynamic(d) = parse_identifier("@overlay=nuclei") else {
            panic!("expected Dynamic");
        };
        let l = d.label.unwrap();
        assert!(l.overlay);
        assert_eq!(l.opacity, None);
    }

    /// A third field on `label=` is not silently dropped: it lands in the palette slot, where the
    /// renderer rejects it. Silently ignoring part of an identifier would serve one image under
    /// another's URL.
    #[test]
    fn a_stray_third_field_on_label_is_not_dropped() {
        let ProjectionId::Dynamic(d) = parse_identifier("@label=nuclei:table:0.5") else {
            panic!("expected Dynamic");
        };
        assert_eq!(d.label.unwrap().palette.as_deref(), Some("table:0.5"));
    }

    #[test]
    fn parses_dynamic() {
        let p = parse_identifier("@z=14,t=0,c=1:cyan:100-4000,c=3:magenta:200-8000");
        match p {
            ProjectionId::Dynamic(d) => {
                assert_eq!(d.z, Some(14));
                assert_eq!(d.t, Some(0));
                assert_eq!(d.channels.len(), 2);
                assert_eq!(
                    d.channels[0],
                    DynChannel {
                        index: 1,
                        color: Some("cyan".into()),
                        window: Some((100.0, 4000.0))
                    }
                );
                assert_eq!(d.channels[1].index, 3);
            }
            _ => panic!("expected Dynamic"),
        }
    }
}

#[cfg(test)]
mod proptests {
    use super::*;
    use proptest::prelude::*;

    proptest! {
        /// `parse_identifier` must never panic on ANY input string — untrusted HTTP path
        /// segments (the IIIF identifier) reach this parser directly.
        #[test]
        fn parse_identifier_never_panics(s in "\\PC*") {
            let _ = parse_identifier(&s);
        }

        /// For a well-formed `@z=..,t=..,c=IDX[:COLOR[:LO-HI]],...` identifier built from
        /// arbitrary-but-valid components, the parsed `z`/`t`/channel indices/colors/
        /// windows are exactly what was generated — the grammar round-trips its own
        /// invariants.
        #[test]
        fn dynamic_proj_round_trips_generated_fields(spec in arb_dynamic_spec()) {
            let id = format!("@{}", spec.to_query_string());
            let parsed = parse_identifier(&id);
            match parsed {
                ProjectionId::Dynamic(d) => {
                    prop_assert_eq!(d.z, spec.z);
                    prop_assert_eq!(d.t, spec.t);
                    prop_assert_eq!(d.channels.len(), spec.channels.len());
                    for (got, want) in d.channels.iter().zip(spec.channels.iter()) {
                        prop_assert_eq!(got.index, want.index);
                        prop_assert_eq!(&got.color, &want.color);
                        prop_assert_eq!(got.window, want.window);
                    }
                }
                other => prop_assert!(false, "expected Dynamic, got {other:?}"),
            }
        }
    }

    #[derive(Debug, Clone)]
    struct ArbChannel {
        index: usize,
        color: Option<String>,
        window: Option<(f64, f64)>,
    }

    #[derive(Debug, Clone)]
    struct ArbDynamicSpec {
        z: Option<u64>,
        t: Option<u64>,
        channels: Vec<ArbChannel>,
    }

    impl ArbDynamicSpec {
        /// Renders this spec to the `z=..,t=..,c=IDX:COLOR:LO-HI,...` query body (without
        /// the leading `@`), matching exactly what `parse_identifier`'s `@`-stripped
        /// grammar expects.
        fn to_query_string(&self) -> String {
            let mut parts = Vec::new();
            if let Some(z) = self.z {
                parts.push(format!("z={z}"));
            }
            if let Some(t) = self.t {
                parts.push(format!("t={t}"));
            }
            for c in &self.channels {
                let mut s = format!("c={}", c.index);
                if let Some(color) = &c.color {
                    s.push(':');
                    s.push_str(color);
                    if let Some((lo, hi)) = c.window {
                        s.push(':');
                        s.push_str(&format!("{lo}-{hi}"));
                    }
                } else if c.window.is_some() {
                    // The grammar requires COLOR before LO-HI positionally — a window
                    // without a color isn't expressible, so this combination isn't
                    // generated (see `arb_channel` below).
                    unreachable!("window without color is not a representable c= form");
                }
                parts.push(s);
            }
            parts.join(",")
        }
    }

    /// A color token drawn from the identifier grammar's actual alphabet: named colors
    /// (`cyan`) or bare hex-ish words. Restricted to ASCII alphanumerics with no `:`/`,`/
    /// `-` so it can't be misparsed as a window or another parameter.
    fn arb_color() -> impl Strategy<Value = String> {
        "[a-zA-Z][a-zA-Z0-9]{0,9}"
    }

    /// A finite, non-negative window endpoint, formatted as a plain (non-scientific)
    /// decimal so it round-trips through `f64::parse` unchanged in string form.
    fn arb_window_endpoint() -> impl Strategy<Value = f64> {
        (0u32..=1_000_000).prop_map(|n| n as f64 / 10.0)
    }

    fn arb_channel() -> impl Strategy<Value = ArbChannel> {
        (
            0usize..64,
            proptest::option::of(arb_color()),
            arb_window_endpoint(),
            arb_window_endpoint(),
        )
            .prop_map(|(index, color, a, b)| {
                let (lo, hi) = if a <= b { (a, b) } else { (b, a) };
                // A window is only representable (and thus only generated) alongside a
                // color, since `c=IDX:LO-HI` with no color slot isn't part of the grammar.
                let window = color.as_ref().map(|_| (lo, hi));
                ArbChannel {
                    index,
                    color,
                    window,
                }
            })
    }

    fn arb_dynamic_spec() -> impl Strategy<Value = ArbDynamicSpec> {
        (
            proptest::option::of(0u64..1000),
            proptest::option::of(0u64..1000),
            proptest::collection::vec(arb_channel(), 0..5),
        )
            .prop_map(|(z, t, channels)| ArbDynamicSpec { z, t, channels })
    }
}
