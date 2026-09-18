use crate::error::ZarrError;
use serde_json::{Map, Value};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AxisKind {
    T,
    C,
    Z,
    Y,
    X,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AxesModel {
    pub order: Vec<AxisKind>,
}

impl AxesModel {
    pub fn index_of(&self, k: AxisKind) -> Option<usize> {
        self.order.iter().position(|&a| a == k)
    }
    pub fn ndim(&self) -> usize {
        self.order.len()
    }
}

#[derive(Debug, Clone)]
pub struct MultiscaleInfo {
    pub axes: AxesModel,
    pub level_paths: Vec<String>,
}

fn axis_kind(name: &str, ty: Option<&str>) -> Result<AxisKind, ZarrError> {
    match ty {
        Some("time") => Ok(AxisKind::T),
        Some("channel") => Ok(AxisKind::C),
        _ => match name.to_ascii_lowercase().as_str() {
            "t" => Ok(AxisKind::T),
            "c" => Ok(AxisKind::C),
            "z" => Ok(AxisKind::Z),
            "y" => Ok(AxisKind::Y),
            "x" => Ok(AxisKind::X),
            _ => Err(ZarrError::UnknownAxis(name.to_string())),
        },
    }
}

/// Recognized OME-Zarr multiscales versions. Anything else declared explicitly is rejected.
const SUPPORTED_VERSIONS: &[&str] = &["0.4", "0.5"];

fn check_version(v: Option<&str>) -> Result<(), ZarrError> {
    match v {
        Some(v) if !SUPPORTED_VERSIONS.contains(&v) => {
            Err(ZarrError::UnsupportedVersion(v.to_string()))
        }
        _ => Ok(()),
    }
}

pub fn parse_multiscale(attributes: &Map<String, Value>) -> Result<MultiscaleInfo, ZarrError> {
    // Try 0.5: attributes = { "ome": { "version": "0.5", "multiscales": [...] } }
    if let Some(ome) = attributes.get("ome") {
        if let Some(ms) = ome.get("multiscales").and_then(|v| v.as_array()) {
            let version = ome.get("version").and_then(|v| v.as_str());
            check_version(version)?;
            return from_multiscales_array(ms);
        }
    }
    // Try 0.4: attributes = { "multiscales": [...] } (top-level), version either at the
    // top level or on the first multiscale entry itself.
    if let Some(ms) = attributes.get("multiscales").and_then(|v| v.as_array()) {
        let top_version = attributes.get("version").and_then(|v| v.as_str());
        let entry_version = ms
            .first()
            .and_then(|m| m.get("version"))
            .and_then(|v| v.as_str());
        check_version(top_version.or(entry_version))?;
        return from_multiscales_array(ms);
    }
    Err(ZarrError::NoMultiscales)
}

fn from_multiscales_array(ms: &[Value]) -> Result<MultiscaleInfo, ZarrError> {
    let first = ms.first().ok_or(ZarrError::NoMultiscales)?;
    let axes_json = first
        .get("axes")
        .and_then(|v| v.as_array())
        .ok_or(ZarrError::NoMultiscales)?;
    let order = axes_json
        .iter()
        .map(|a| {
            let name = a.get("name").and_then(|v| v.as_str()).ok_or_else(|| {
                ZarrError::UnknownAxis("axis object missing a string 'name' field".to_string())
            })?;
            let ty = a.get("type").and_then(|v| v.as_str());
            axis_kind(name, ty)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let datasets = first
        .get("datasets")
        .and_then(|v| v.as_array())
        .ok_or(ZarrError::NoMultiscales)?;
    let level_paths = datasets
        .iter()
        .filter_map(|d| d.get("path").and_then(|v| v.as_str()).map(String::from))
        .collect::<Vec<_>>();
    if level_paths.is_empty() {
        return Err(ZarrError::NoMultiscales);
    }
    Ok(MultiscaleInfo {
        axes: AxesModel { order },
        level_paths,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_ome_0_5_multiscale() {
        let attrs = json!({
            "ome": { "version": "0.5", "multiscales": [{
                "axes": [
                    {"name":"t","type":"time"}, {"name":"c","type":"channel"},
                    {"name":"z","type":"space"}, {"name":"y","type":"space"}, {"name":"x","type":"space"}
                ],
                "datasets": [{"path":"0"},{"path":"1"},{"path":"2"}]
            }]}
        });
        let info = parse_multiscale(attrs.as_object().unwrap()).unwrap();
        assert_eq!(
            info.axes.order,
            vec![
                AxisKind::T,
                AxisKind::C,
                AxisKind::Z,
                AxisKind::Y,
                AxisKind::X
            ]
        );
        assert_eq!(info.level_paths, vec!["0", "1", "2"]);
        assert_eq!(info.axes.index_of(AxisKind::C), Some(1));
    }

    #[test]
    fn parses_ome_0_4_top_level_multiscale() {
        let attrs = json!({
            "multiscales": [{
                "axes": [{"name":"y","type":"space"},{"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        let info = parse_multiscale(attrs.as_object().unwrap()).unwrap();
        assert_eq!(info.axes.order, vec![AxisKind::Y, AxisKind::X]);
        assert_eq!(info.level_paths, vec!["0"]);
    }

    #[test]
    fn errors_when_no_multiscales() {
        let attrs = json!({"foo": 1});
        assert!(matches!(
            parse_multiscale(attrs.as_object().unwrap()),
            Err(ZarrError::NoMultiscales)
        ));
    }

    #[test]
    fn axis_kind_rejects_unknown_name_and_type() {
        let err = axis_kind("foo", Some("space")).unwrap_err();
        assert!(matches!(err, ZarrError::UnknownAxis(ref n) if n == "foo"));
    }

    #[test]
    fn axis_kind_recognizes_by_name_case_insensitive() {
        assert_eq!(axis_kind("T", None).unwrap(), AxisKind::T);
        assert_eq!(axis_kind("C", None).unwrap(), AxisKind::C);
        assert_eq!(axis_kind("Z", None).unwrap(), AxisKind::Z);
        assert_eq!(axis_kind("Y", None).unwrap(), AxisKind::Y);
        assert_eq!(axis_kind("X", None).unwrap(), AxisKind::X);
    }

    #[test]
    fn axis_kind_recognizes_by_type() {
        assert_eq!(axis_kind("anything", Some("time")).unwrap(), AxisKind::T);
        assert_eq!(axis_kind("anything", Some("channel")).unwrap(), AxisKind::C);
    }

    #[test]
    fn parse_multiscale_propagates_unknown_axis() {
        let attrs = json!({
            "multiscales": [{
                "axes": [{"name":"y","type":"space"},{"name":"foo","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        let err = parse_multiscale(attrs.as_object().unwrap()).unwrap_err();
        assert!(matches!(err, ZarrError::UnknownAxis(ref n) if n == "foo"));
    }

    /// An axis object with no `name` field must be a hard parse error, not a silent default to
    /// "x" — a missing `name` is malformed metadata, and silently treating it as the X axis would
    /// mask that malformation (combined with `axis_kind`'s reject-unknown-names behavior, letting
    /// a nameless axis quietly through as X was the one remaining silent-wrong-render path).
    #[test]
    fn missing_axis_name_is_an_error_not_a_silent_x_default() {
        let attrs = json!({
            "multiscales": [{
                "axes": [{"type":"space"}, {"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        let err = parse_multiscale(attrs.as_object().unwrap()).unwrap_err();
        assert!(matches!(err, ZarrError::UnknownAxis(ref m) if m.contains("missing")));
    }

    /// Same as above, but `name` is present with the WRONG type (a number, not a string) —
    /// also must fail loud rather than `.as_str()` silently returning `None` and defaulting.
    #[test]
    fn non_string_axis_name_is_an_error_not_a_silent_x_default() {
        let attrs = json!({
            "multiscales": [{
                "axes": [{"name": 123, "type":"space"}, {"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        let err = parse_multiscale(attrs.as_object().unwrap()).unwrap_err();
        assert!(matches!(err, ZarrError::UnknownAxis(ref m) if m.contains("missing")));
    }

    #[test]
    fn rejects_unsupported_version_0_3() {
        let attrs = json!({
            "multiscales": [{
                "version": "0.3",
                "axes": [{"name":"y","type":"space"},{"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        let err = parse_multiscale(attrs.as_object().unwrap()).unwrap_err();
        assert!(matches!(err, ZarrError::UnsupportedVersion(ref v) if v == "0.3"));
    }

    #[test]
    fn rejects_unsupported_version_0_2_top_level() {
        let attrs = json!({
            "version": "0.2",
            "multiscales": [{
                "axes": [{"name":"y","type":"space"},{"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        let err = parse_multiscale(attrs.as_object().unwrap()).unwrap_err();
        assert!(matches!(err, ZarrError::UnsupportedVersion(ref v) if v == "0.2"));
    }

    #[test]
    fn accepts_declared_version_0_4() {
        let attrs = json!({
            "multiscales": [{
                "version": "0.4",
                "axes": [{"name":"y","type":"space"},{"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        assert!(parse_multiscale(attrs.as_object().unwrap()).is_ok());
    }

    #[test]
    fn accepts_declared_version_0_5_via_ome_attribute() {
        let attrs = json!({
            "ome": { "version": "0.5", "multiscales": [{
                "axes": [{"name":"y","type":"space"},{"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]}
        });
        assert!(parse_multiscale(attrs.as_object().unwrap()).is_ok());
    }

    #[test]
    fn absent_version_is_lenient() {
        // Many 0.4 files omit an explicit version field; must still parse.
        let attrs = json!({
            "multiscales": [{
                "axes": [{"name":"y","type":"space"},{"name":"x","type":"space"}],
                "datasets": [{"path":"0"}]
            }]
        });
        assert!(parse_multiscale(attrs.as_object().unwrap()).is_ok());
    }
}
