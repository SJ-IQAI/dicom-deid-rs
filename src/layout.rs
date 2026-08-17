//! Output path layouts built from de-identified tag values.
//!
//! Input directory trees are usually organised by identifiers that are
//! themselves PHI (`<PatientID>/<StudyInstanceUID>/...`). Mirroring that
//! tree into the output directory would re-introduce the PHI that the
//! header actions just removed, so an [`OutputLayout`] rebuilds the path
//! from the *de-identified* data set instead.
//!
//! A layout is a `/`-separated template whose `{Token}` placeholders name
//! DICOM tags, e.g.
//!
//! ```text
//! {PatientID}/{StudyInstanceUID}/{SeriesInstanceUID}_{SeriesNumber}/{SOPInstanceUID}.dcm
//! ```
//!
//! Tokens are resolved once, at parse time, so an unknown keyword is a
//! start-up error rather than a per-file failure. Rendering happens after
//! the header actions and the file meta sync have run, so the values read
//! back out of the object are the de-identified ones.

use crate::error::DeidError;
use crate::tag::{parse_bare_hex_tag, parse_parenthesized_tag};
use dicom_core::Tag;
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_dictionary_std::StandardDataDictionary;
use dicom_object::InMemDicomObject;
use std::path::PathBuf;

/// The canonical de-identified layout, mirroring the common
/// `<PatientID>/<StudyInstanceUID>/<SeriesInstanceUID>_<SeriesNumber>/<SOPInstanceUID>.dcm`
/// input structure with de-identified values.
pub const DEID_PATH_LAYOUT: &str =
    "{PatientID}/{StudyInstanceUID}/{SeriesInstanceUID}_{SeriesNumber}/{SOPInstanceUID}.dcm";

/// The maximum length of a single path component, in bytes.
///
/// 255 is the limit on every filesystem this tool targets (ext4, APFS,
/// NTFS, XFS).
const MAX_COMPONENT_LEN: usize = 255;

/// Base names Windows refuses to create, regardless of extension.
const WINDOWS_RESERVED: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];

/// One piece of a path component: either fixed text from the template or
/// a tag whose value is read from the data set.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Literal(String),
    Tag { tag: Tag, token: String },
}

/// A parsed output path template.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputLayout {
    components: Vec<Vec<Segment>>,
}

impl OutputLayout {
    /// Parse a `/`-separated layout template.
    ///
    /// Every `{Token}` is resolved to a concrete tag immediately, so
    /// typos surface before any file is processed. Literal text is
    /// restricted to `[A-Za-z0-9._-]` so that a template cannot smuggle
    /// in a path separator or a `..` component.
    pub fn parse(template: &str) -> Result<Self, DeidError> {
        if template.trim().is_empty() {
            return Err(DeidError::Layout("layout template is empty".into()));
        }
        if template.contains('\\') {
            return Err(DeidError::Layout(
                "layout template must use '/' as the path separator".into(),
            ));
        }
        if template.starts_with('/') {
            return Err(DeidError::Layout(
                "layout template must be relative to the output directory".into(),
            ));
        }

        let mut components = Vec::new();
        for part in template.split('/') {
            if part.is_empty() {
                return Err(DeidError::Layout(format!(
                    "layout template has an empty path component: {}",
                    template
                )));
            }
            components.push(parse_component(part)?);
        }

        // A layout made only of literals would send every file to the
        // same path, which the collision check would then reject file by
        // file. Catching it here gives a far clearer message.
        if !components
            .iter()
            .any(|c| c.iter().any(|s| matches!(s, Segment::Tag { .. })))
        {
            return Err(DeidError::Layout(
                "layout template contains no {Tag} placeholders; every file would \
                 resolve to the same path"
                    .into(),
            ));
        }

        Ok(OutputLayout { components })
    }

    /// The distinct tags this layout reads, in first-appearance order.
    pub fn tags(&self) -> Vec<Tag> {
        let mut out: Vec<Tag> = Vec::new();
        for component in &self.components {
            for segment in component {
                if let Segment::Tag { tag, .. } = segment
                    && !out.contains(tag)
                {
                    out.push(*tag);
                }
            }
        }
        out
    }

    /// Render the layout against a de-identified data set.
    ///
    /// Returns a relative path with no `..` components and no embedded
    /// separators beyond the ones the template itself declares, so
    /// joining it onto the output directory cannot escape that
    /// directory.
    ///
    /// A tag that is absent, unreadable, or empty after padding is
    /// trimmed yields [`DeidError::Layout`], which is non-fatal: the
    /// caller counts the file as skipped per r-1-5.
    pub fn render(&self, obj: &InMemDicomObject) -> Result<PathBuf, DeidError> {
        let mut path = PathBuf::new();
        for component in &self.components {
            let mut rendered = String::new();
            for segment in component {
                match segment {
                    Segment::Literal(text) => rendered.push_str(text),
                    Segment::Tag { tag, token } => {
                        rendered.push_str(&sanitize_value(&read_tag_value(obj, *tag, token)?));
                    }
                }
            }
            path.push(finalize_component(&rendered)?);
        }
        Ok(path)
    }
}

/// Read a tag's value as a trimmed string, or explain why it cannot be
/// used in a path.
fn read_tag_value(obj: &InMemDicomObject, tag: Tag, token: &str) -> Result<String, DeidError> {
    let elem = obj.element(tag).map_err(|_| {
        DeidError::Layout(format!(
            "layout tag {} ({:04X},{:04X}) is absent from the de-identified data set",
            token, tag.0, tag.1
        ))
    })?;
    let raw = elem.value().to_str().map_err(|e| {
        DeidError::Layout(format!(
            "layout tag {} ({:04X},{:04X}) cannot be read as text: {}",
            token, tag.0, tag.1, e
        ))
    })?;
    // DICOM pads string values to an even length with a trailing space
    // (or NUL for UI), and IS/DS values may carry leading whitespace.
    let trimmed = raw.trim_matches(|c: char| c == ' ' || c == '\0' || c.is_whitespace());
    if trimmed.is_empty() {
        return Err(DeidError::Layout(format!(
            "layout tag {} ({:04X},{:04X}) is empty in the de-identified data set",
            token, tag.0, tag.1
        )));
    }
    Ok(trimmed.to_string())
}

/// Whether a character may appear verbatim in a path component.
fn is_safe_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// Reduce a tag value to characters that are safe in a path component.
///
/// Anything outside `[A-Za-z0-9._-]` — path separators, control
/// characters, non-ASCII text — becomes `_`, and runs of replacements
/// collapse into a single `_` so a fully-substituted value does not turn
/// into a wall of underscores. This is a total function: no input can
/// produce a separator, so no input can traverse out of the output
/// directory.
fn sanitize_value(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    let mut last_was_replacement = false;
    for c in value.chars() {
        if is_safe_char(c) {
            out.push(c);
            last_was_replacement = false;
        } else if !last_was_replacement {
            out.push('_');
            last_was_replacement = true;
        }
    }
    out
}

/// Apply the whole-component rules that per-value sanitization cannot:
/// relative-path names, Windows quirks, and length limits.
fn finalize_component(component: &str) -> Result<String, DeidError> {
    // Windows silently strips trailing dots and spaces, which would make
    // two distinct components collide.
    let trimmed = component.trim_end_matches(['.', ' ']);

    if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        return Err(DeidError::Layout(format!(
            "layout produced the unusable path component {:?}",
            component
        )));
    }

    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let out = if WINDOWS_RESERVED
        .iter()
        .any(|r| r.eq_ignore_ascii_case(stem))
    {
        format!("_{}", trimmed)
    } else {
        trimmed.to_string()
    };

    if out.len() > MAX_COMPONENT_LEN {
        return Err(DeidError::Layout(format!(
            "layout produced a {}-byte path component, over the {}-byte filesystem limit: {:?}",
            out.len(),
            MAX_COMPONENT_LEN,
            out
        )));
    }

    Ok(out)
}

/// Split one path component of the template into literal and tag segments.
fn parse_component(part: &str) -> Result<Vec<Segment>, DeidError> {
    let mut segments = Vec::new();
    let mut literal = String::new();
    let mut rest = part;

    while !rest.is_empty() {
        match rest.find('{') {
            Some(open) => {
                literal.push_str(&rest[..open]);
                let after = &rest[open + 1..];
                let close = after.find('}').ok_or_else(|| {
                    DeidError::Layout(format!("unclosed '{{' in layout component: {}", part))
                })?;
                let token = &after[..close];
                if token.contains('{') {
                    return Err(DeidError::Layout(format!(
                        "nested '{{' in layout component: {}",
                        part
                    )));
                }
                if !literal.is_empty() {
                    segments.push(Segment::Literal(validate_literal(&literal, part)?));
                    literal.clear();
                }
                segments.push(Segment::Tag {
                    tag: resolve_token(token)?,
                    token: token.to_string(),
                });
                rest = &after[close + 1..];
            }
            None => {
                literal.push_str(rest);
                rest = "";
            }
        }
    }

    if !literal.is_empty() {
        segments.push(Segment::Literal(validate_literal(&literal, part)?));
    }
    Ok(segments)
}

/// Literal template text is author-controlled, so it is rejected rather
/// than sanitized: a stray `..` or separator is a mistake worth
/// reporting, not something to silently rewrite.
fn validate_literal(literal: &str, part: &str) -> Result<String, DeidError> {
    if literal.contains('}') {
        return Err(DeidError::Layout(format!(
            "unmatched '}}' in layout component: {}",
            part
        )));
    }
    if let Some(bad) = literal.chars().find(|c| !is_safe_char(*c)) {
        return Err(DeidError::Layout(format!(
            "literal text in a layout component may only use [A-Za-z0-9._-], found {:?} in: {}",
            bad, part
        )));
    }
    Ok(literal.to_string())
}

/// Resolve a `{Token}` to a tag: a keyword, `(gggg,eeee)`, or bare hex.
fn resolve_token(token: &str) -> Result<Tag, DeidError> {
    let token = token.trim();
    if token.is_empty() {
        return Err(DeidError::Layout("empty '{}' placeholder in layout".into()));
    }
    if token.starts_with('(') {
        return parse_parenthesized_tag(token)
            .map_err(|e| DeidError::Layout(format!("in layout placeholder: {}", e)));
    }
    if token.len() == 8 && token.chars().all(|c| c.is_ascii_hexdigit()) {
        return parse_bare_hex_tag(token)
            .map_err(|e| DeidError::Layout(format!("in layout placeholder: {}", e)));
    }
    StandardDataDictionary
        .by_name(token)
        .map(|entry| entry.tag())
        .ok_or_else(|| {
            DeidError::Layout(format!(
                "unknown tag keyword in layout placeholder: {}",
                token
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use dicom_core::VR;
    use dicom_dictionary_std::tags;

    fn layout_obj() -> InMemDicomObject {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "abc123");
        put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, "2.25.1111");
        put_str(&mut obj, tags::SERIES_INSTANCE_UID, VR::UI, "2.25.2222");
        put_str(&mut obj, tags::SERIES_NUMBER, VR::IS, "3");
        put_str(&mut obj, tags::SOP_INSTANCE_UID, VR::UI, "2.25.3333");
        obj
    }

    // -- r-1-6 ---------------------------------------------------------------

    /// Requirement r-1-6
    #[test]
    fn r1_6_renders_the_canonical_layout() {
        let layout = OutputLayout::parse(DEID_PATH_LAYOUT).expect("should parse");
        let path = layout.render(&layout_obj()).expect("should render");
        assert_eq!(
            path,
            PathBuf::from("abc123")
                .join("2.25.1111")
                .join("2.25.2222_3")
                .join("2.25.3333.dcm")
        );
    }

    /// Requirement r-1-6
    #[test]
    fn r1_6_accepts_tag_number_placeholders() {
        let layout = OutputLayout::parse("{(0010,0020)}/{00080018}.dcm").expect("should parse");
        let path = layout.render(&layout_obj()).expect("should render");
        assert_eq!(path, PathBuf::from("abc123").join("2.25.3333.dcm"));
    }

    /// Requirement r-1-6
    #[test]
    fn r1_6_rejects_unknown_keyword_at_parse_time() {
        let err = OutputLayout::parse("{NotATag}/x.dcm").expect_err("should reject");
        assert!(err.to_string().contains("unknown tag keyword"));
    }

    /// Requirement r-1-6
    #[test]
    fn r1_6_rejects_malformed_templates() {
        for template in [
            "",
            "{PatientID",
            "PatientID}",
            "/{PatientID}",
            "{PatientID}//{SOPInstanceUID}.dcm",
            "{PatientID}\\{SOPInstanceUID}.dcm",
            "{}/x.dcm",
            "fixed/path.dcm",
        ] {
            assert!(
                OutputLayout::parse(template).is_err(),
                "should reject template {:?}",
                template
            );
        }
    }

    /// Requirement r-1-6
    #[test]
    fn r1_6_reports_the_tags_it_reads() {
        let layout = OutputLayout::parse(DEID_PATH_LAYOUT).expect("should parse");
        assert_eq!(
            layout.tags(),
            vec![
                tags::PATIENT_ID,
                tags::STUDY_INSTANCE_UID,
                tags::SERIES_INSTANCE_UID,
                tags::SERIES_NUMBER,
                tags::SOP_INSTANCE_UID,
            ]
        );
    }

    /// Requirement r-1-6: trailing padding must not reach the path.
    #[test]
    fn r1_6_trims_dicom_padding() {
        let mut obj = layout_obj();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "abc123 ");
        put_str(&mut obj, tags::SERIES_NUMBER, VR::IS, " 3 ");
        let layout = OutputLayout::parse("{PatientID}/{SeriesNumber}.dcm").expect("should parse");
        let path = layout.render(&obj).expect("should render");
        assert_eq!(path, PathBuf::from("abc123").join("3.dcm"));
    }

    // -- r-1-7 ---------------------------------------------------------------

    /// Requirement r-1-7: a tag value can never introduce a separator.
    #[test]
    fn r1_7_sanitizes_path_separators_out_of_values() {
        let mut obj = layout_obj();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "../../etc/passwd");
        let layout = OutputLayout::parse("{PatientID}/{SOPInstanceUID}.dcm").expect("should parse");
        let path = layout.render(&obj).expect("should render");

        // The traversal is flattened into ONE inert component rather
        // than several, so joining onto the output directory cannot
        // escape it.
        assert_eq!(
            path,
            PathBuf::from(".._.._etc_passwd").join("2.25.3333.dcm")
        );
        assert_eq!(path.components().count(), 2);
        assert!(
            !path
                .components()
                .any(|c| c.as_os_str() == ".." || c.as_os_str() == "."),
            "no component may be a relative-path directive"
        );
    }

    /// Requirement r-1-7
    #[test]
    fn r1_7_rejects_dot_components() {
        let layout = OutputLayout::parse("{PatientID}/{SOPInstanceUID}").expect("should parse");
        for value in ["..", ".", "...", "..  "] {
            let mut obj = layout_obj();
            put_str(&mut obj, tags::PATIENT_ID, VR::LO, value);
            let err = layout.render(&obj).expect_err("should reject");
            assert!(err.to_string().contains("unusable path component"));
        }
    }

    /// Requirement r-1-7
    #[test]
    fn r1_7_replaces_unsafe_characters_and_collapses_runs() {
        let mut obj = layout_obj();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "Doe^Jane   R\u{7}s");
        let layout = OutputLayout::parse("{PatientID}/{SOPInstanceUID}.dcm").expect("should parse");
        let path = layout.render(&obj).expect("should render");
        assert_eq!(path, PathBuf::from("Doe_Jane_R_s").join("2.25.3333.dcm"));
    }

    /// Requirement r-1-7
    #[test]
    fn r1_7_escapes_windows_reserved_names() {
        let mut obj = layout_obj();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "con");
        let layout = OutputLayout::parse("{PatientID}/{SOPInstanceUID}.dcm").expect("should parse");
        let path = layout.render(&obj).expect("should render");
        assert_eq!(path, PathBuf::from("_con").join("2.25.3333.dcm"));
    }

    /// Requirement r-1-7
    #[test]
    fn r1_7_rejects_overlong_components() {
        let mut obj = layout_obj();
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, &"a".repeat(300));
        let layout = OutputLayout::parse("{PatientID}/{SOPInstanceUID}.dcm").expect("should parse");
        let err = layout.render(&obj).expect_err("should reject");
        assert!(err.to_string().contains("filesystem limit"));
    }

    // -- r-1-8 ---------------------------------------------------------------

    /// Requirement r-1-8
    #[test]
    fn r1_8_missing_tag_is_a_non_fatal_error() {
        let mut obj = layout_obj();
        let _ = obj.remove_element(tags::PATIENT_ID);
        let layout = OutputLayout::parse(DEID_PATH_LAYOUT).expect("should parse");
        let err = layout.render(&obj).expect_err("should fail");
        assert!(err.to_string().contains("absent"));
        assert!(!err.is_fatal(), "must be counted as a skip, not an abort");
    }

    /// Requirement r-1-8
    #[test]
    fn r1_8_empty_tag_is_a_non_fatal_error() {
        let mut obj = layout_obj();
        put_empty(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI);
        let layout = OutputLayout::parse(DEID_PATH_LAYOUT).expect("should parse");
        let err = layout.render(&obj).expect_err("should fail");
        assert!(err.to_string().contains("empty"));
        assert!(!err.is_fatal(), "must be counted as a skip, not an abort");
    }

    /// Requirement r-1-8: a value that is only padding counts as empty.
    #[test]
    fn r1_8_whitespace_only_tag_is_an_error() {
        let mut obj = layout_obj();
        put_str(&mut obj, tags::STUDY_INSTANCE_UID, VR::UI, "   ");
        let layout = OutputLayout::parse(DEID_PATH_LAYOUT).expect("should parse");
        let err = layout.render(&obj).expect_err("should fail");
        assert!(err.to_string().contains("empty"));
    }
}
