//! A validated image name.
//!
//! Names are the only thing a request may use to ask for an image, and they are deliberately NOT
//! paths. A name may look like one (`idr0062A/6001240`), and a lazy directory source derives one
//! from a path, but the two are different kinds of thing: a name is an identity in the URL space,
//! a path is a location on a disk. Keeping the distinction is what lets an explicit catalogue
//! publish a stable, citable name for an image whose storage moves.
//!
//! Validation here is about the NAME being well formed. It is not, and cannot be, sufficient to
//! make a filesystem lookup safe: a name with no `..` in it can still resolve through a symlink
//! out of a root. Confinement is the directory source's job (a later phase) and is done by
//! canonicalising the resolved path, not by trusting this check.
use std::fmt;

/// Upper bound on a name, in bytes.
///
/// Generous because a caller-named remote store (a later phase) puts a whole store URL inside the
/// name, and the longest realistic one, a deeply nested IDR S3 key, is around 120 bytes. 512
/// leaves room for several times that while still refusing a name built to bloat a cache key.
pub const MAX_NAME_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum NameError {
    #[error("image name is empty")]
    Empty,
    #[error("image name has an empty path segment")]
    EmptySegment,
    #[error("image name segment {0:?} is a relative path component")]
    RelativeSegment(String),
    #[error("image name is {0} bytes, over the {MAX_NAME_BYTES}-byte limit")]
    TooLong(usize),
    #[error("image name contains a control character")]
    ControlCharacter,
}

/// A well-formed image name: one or more non-empty, non-relative, control-character-free segments
/// joined by `/`.
///
/// Slashes are kept unencoded, which is the point of the `/i/{name}/` mount. Every other IIIF
/// server puts the image in the identifier slot, where a nested name has to be percent-encoded as
/// `%2F`, and `%2F` does not survive many reverse proxies. See the design spec for the argument.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ImageName(String);

impl ImageName {
    pub fn parse(s: &str) -> Result<Self, NameError> {
        if s.is_empty() {
            return Err(NameError::Empty);
        }
        if s.len() > MAX_NAME_BYTES {
            return Err(NameError::TooLong(s.len()));
        }
        if s.chars().any(char::is_control) {
            return Err(NameError::ControlCharacter);
        }
        for segment in s.split('/') {
            if segment.is_empty() {
                return Err(NameError::EmptySegment);
            }
            if segment == "." || segment == ".." {
                return Err(NameError::RelativeSegment(segment.to_string()));
            }
        }
        Ok(ImageName(s.to_string()))
    }

    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for ImageName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_a_single_segment() {
        assert_eq!(ImageName::parse("nuclei").unwrap().as_str(), "nuclei");
    }

    /// The whole reason the mount exists: nested names need no percent-encoding.
    #[test]
    fn accepts_nested_segments() {
        let n = ImageName::parse("idr0062A/6001240").unwrap();
        assert_eq!(n.as_str(), "idr0062A/6001240");
    }

    #[test]
    fn rejects_empty_and_empty_segments() {
        assert_eq!(ImageName::parse(""), Err(NameError::Empty));
        assert_eq!(ImageName::parse("a//b"), Err(NameError::EmptySegment));
        assert_eq!(ImageName::parse("/a"), Err(NameError::EmptySegment));
        assert_eq!(ImageName::parse("a/"), Err(NameError::EmptySegment));
    }

    /// Traversal is rejected at the name, and AGAIN at path resolution (a later phase). Neither
    /// check is sufficient alone: this one cannot see symlinks, and that one cannot see a name
    /// that never touches a filesystem.
    #[test]
    fn rejects_relative_segments() {
        for bad in ["..", "a/../b", "./a", "a/."] {
            assert!(
                matches!(ImageName::parse(bad), Err(NameError::RelativeSegment(_))),
                "{bad} should be rejected"
            );
        }
    }

    #[test]
    fn rejects_control_characters() {
        assert_eq!(ImageName::parse("a\nb"), Err(NameError::ControlCharacter));
        assert_eq!(ImageName::parse("a\0b"), Err(NameError::ControlCharacter));
    }

    #[test]
    fn rejects_over_length_names() {
        let long = "a".repeat(MAX_NAME_BYTES + 1);
        assert_eq!(
            ImageName::parse(&long),
            Err(NameError::TooLong(MAX_NAME_BYTES + 1))
        );
    }

    /// A name that ends in a reserved marker is FINE, because `crate::mount` splits on the last
    /// occurrence. Locked here so nobody reintroduces a restriction the routing does not need.
    #[test]
    fn a_name_may_end_in_a_reserved_marker() {
        assert!(ImageName::parse("a/iiif").is_ok());
        assert!(ImageName::parse("viewer").is_ok());
    }
}
