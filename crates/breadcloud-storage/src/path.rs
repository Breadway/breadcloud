use std::fmt;

/// A validated, storage-relative path.
///
/// Constructing a `DavPath` via [`DavPath::parse`] is the only way to get
/// one from untrusted input, and parsing rejects anything that could
/// escape the storage root: `..` segments, embedded NUL bytes, and so on.
/// Every other part of this crate can assume a `DavPath` is safe to join
/// onto the storage root without a traversal check.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DavPath {
    segments: Vec<String>,
}

impl DavPath {
    /// The storage root itself ("/").
    pub fn root() -> Self {
        DavPath {
            segments: Vec::new(),
        }
    }

    /// Parse an already percent-decoded request path (e.g. `/foo/bar.txt`)
    /// into a validated `DavPath`.
    ///
    /// Repeated and leading/trailing slashes are collapsed, `.` segments
    /// are dropped, and `..` segments or embedded NUL bytes are rejected.
    pub fn parse(decoded_path: &str) -> Result<Self, InvalidPath> {
        let mut segments = Vec::new();
        for seg in decoded_path.split('/') {
            if seg.is_empty() || seg == "." {
                continue;
            }
            if seg == ".." {
                return Err(InvalidPath(decoded_path.to_string()));
            }
            if seg.contains('\0') {
                return Err(InvalidPath(decoded_path.to_string()));
            }
            segments.push(seg.to_string());
        }
        Ok(DavPath { segments })
    }

    pub fn is_root(&self) -> bool {
        self.segments.is_empty()
    }

    pub fn segments(&self) -> &[String] {
        &self.segments
    }

    /// The final path segment (the file or directory's own name), or
    /// `None` at the root.
    pub fn name(&self) -> Option<&str> {
        self.segments.last().map(String::as_str)
    }

    /// The containing collection, or `None` if this path is already root.
    pub fn parent(&self) -> Option<DavPath> {
        if self.segments.is_empty() {
            return None;
        }
        let mut segments = self.segments.clone();
        segments.pop();
        Some(DavPath { segments })
    }

    /// A path for a child of this collection.
    pub fn child(&self, name: &str) -> DavPath {
        let mut segments = self.segments.clone();
        segments.push(name.to_string());
        DavPath { segments }
    }

    /// Render as a `/`-separated absolute path, e.g. `/foo/bar.txt`. Root
    /// renders as `/`.
    pub fn to_request_path(&self) -> String {
        if self.segments.is_empty() {
            "/".to_string()
        } else {
            format!("/{}", self.segments.join("/"))
        }
    }

    /// Whether `self` is `other`, or a collection containing `other` at
    /// any depth. Used to reject moving/copying a collection into its own
    /// descendant.
    pub fn is_ancestor_of(&self, other: &DavPath) -> bool {
        self.segments.len() <= other.segments.len()
            && other.segments[..self.segments.len()] == self.segments[..]
    }
}

impl fmt::Display for DavPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.to_request_path())
    }
}

/// A request path that could not be resolved to a safe storage-relative
/// path (path traversal, embedded NUL, and so on).
#[derive(Debug, Clone, thiserror::Error)]
#[error("invalid path: {0}")]
pub struct InvalidPath(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn collapses_slashes_and_dot_segments() {
        let p = DavPath::parse("//foo/./bar//").unwrap();
        assert_eq!(p.segments(), &["foo", "bar"]);
        assert_eq!(p.to_request_path(), "/foo/bar");
    }

    #[test]
    fn root_parses_to_empty_segments() {
        let p = DavPath::parse("/").unwrap();
        assert!(p.is_root());
        assert_eq!(p.to_request_path(), "/");
    }

    #[test]
    fn rejects_dotdot_traversal() {
        assert!(DavPath::parse("/foo/../../etc/passwd").is_err());
        assert!(DavPath::parse("..").is_err());
    }

    #[test]
    fn rejects_nul_bytes() {
        assert!(DavPath::parse("/foo\0bar").is_err());
    }

    #[test]
    fn parent_and_child_roundtrip() {
        let p = DavPath::parse("/a/b/c").unwrap();
        let parent = p.parent().unwrap();
        assert_eq!(parent.to_request_path(), "/a/b");
        assert_eq!(parent.child("c"), p);
    }

    #[test]
    fn root_has_no_parent() {
        assert!(DavPath::root().parent().is_none());
    }

    #[test]
    fn ancestor_detection() {
        let a = DavPath::parse("/a").unwrap();
        let ab = DavPath::parse("/a/b").unwrap();
        let x = DavPath::parse("/x").unwrap();
        assert!(a.is_ancestor_of(&ab));
        assert!(a.is_ancestor_of(&a));
        assert!(!ab.is_ancestor_of(&a));
        assert!(!a.is_ancestor_of(&x));
    }
}
