use crate::error::DeidError;
use crate::recipe::TagSpecifier;
use dicom_core::Tag;
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::header::Header;
use dicom_dictionary_std::StandardDataDictionary;
use dicom_object::InMemDicomObject;
use regex::Regex;

/// Parse a tag string in parenthesized format "(GGGG,EEEE)" into a `Tag`.
pub fn parse_parenthesized_tag(s: &str) -> Result<Tag, DeidError> {
    let inner = s
        .strip_prefix('(')
        .and_then(|s| s.strip_suffix(')'))
        .ok_or_else(|| DeidError::TagResolution(format!("expected parenthesized tag: {}", s)))?;
    let (group_str, elem_str) = inner
        .split_once(',')
        .ok_or_else(|| DeidError::TagResolution(format!("expected comma in tag: {}", s)))?;
    let group = u16::from_str_radix(group_str.trim(), 16)
        .map_err(|_| DeidError::TagResolution(format!("invalid group: {}", group_str)))?;
    let element = u16::from_str_radix(elem_str.trim(), 16)
        .map_err(|_| DeidError::TagResolution(format!("invalid element: {}", elem_str)))?;
    Ok(Tag(group, element))
}

/// Parse a tag string in bare hex format "GGGGEEEE" into a `Tag`.
pub fn parse_bare_hex_tag(s: &str) -> Result<Tag, DeidError> {
    if s.len() != 8 || !s.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(DeidError::TagResolution(format!(
            "invalid bare hex tag: {}",
            s
        )));
    }
    let group = u16::from_str_radix(&s[0..4], 16)
        .map_err(|_| DeidError::TagResolution(format!("invalid group: {}", &s[0..4])))?;
    let element = u16::from_str_radix(&s[4..8], 16)
        .map_err(|_| DeidError::TagResolution(format!("invalid element: {}", &s[4..8])))?;
    Ok(Tag(group, element))
}

/// Resolve a `TagSpecifier` into one or more concrete `Tag` values.
///
/// For pattern-based specifiers, the object is inspected to find all matching
/// tags. For keyword and direct tag specifiers, the result is a single tag.
pub fn resolve_tags(
    specifier: &TagSpecifier,
    obj: &InMemDicomObject,
) -> Result<Vec<Tag>, DeidError> {
    let dict = StandardDataDictionary;
    match specifier {
        TagSpecifier::Keyword(name) => {
            let entry = dict
                .by_name(name)
                .ok_or_else(|| DeidError::TagResolution(format!("unknown keyword: {}", name)))?;
            Ok(vec![entry.tag()])
        }
        TagSpecifier::TagValue(tag) => Ok(vec![*tag]),
        // Unlike an exact tag, a wildcard has to be resolved against the
        // data set: it stands for whichever of the repeating group's
        // tags this particular file happens to carry.
        TagSpecifier::Wildcard { .. } => Ok(obj
            .iter()
            .map(|elem| elem.tag())
            .filter(|tag| specifier.matches(*tag).unwrap_or(false))
            .collect()),
        TagSpecifier::Pattern(pattern) => {
            let re = Regex::new(pattern)
                .map_err(|e| DeidError::TagResolution(format!("invalid regex: {}", e)))?;
            let mut matched = Vec::new();
            for elem in obj.iter() {
                let tag = elem.tag();
                let tag_str = format!("({:04x},{:04x})", tag.0, tag.1);
                let keyword = dict
                    .by_tag(tag)
                    .map(|e| e.alias().to_string())
                    .unwrap_or_default();
                if re.is_match(&keyword) || re.is_match(&tag_str) {
                    matched.push(tag);
                }
            }
            Ok(matched)
        }
        TagSpecifier::PrivateTag {
            group,
            creator,
            element_offset,
        } => {
            for elem in obj.iter() {
                let tag = elem.tag();
                if tag.0 == *group
                    && (0x0010..=0x00FF).contains(&tag.1)
                    && elem
                        .value()
                        .to_str()
                        .is_ok_and(|val| val.trim() == creator.as_str())
                {
                    let slot = tag.1;
                    let resolved = Tag(*group, (slot << 8) | (*element_offset as u16));
                    return Ok(vec![resolved]);
                }
            }
            Err(DeidError::TagResolution(format!(
                "private creator '{}' not found in group {:04x}",
                creator, group
            )))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_helpers::*;
    use dicom_core::{Tag, VR};
    use dicom_dictionary_std::tags;

    // -- r-3-4-1 -------------------------------------------------------------

    /// Requirement r-3-4-1
    #[test]
    fn r3_4_1_resolve_keyword_patient_id() {
        let obj = create_test_obj();
        let spec = TagSpecifier::Keyword("PatientID".into());
        let tags = resolve_tags(&spec, &obj).expect("should resolve");
        assert_eq!(tags, vec![tags::PATIENT_ID]);
    }

    /// Requirement r-3-4-1
    #[test]
    fn r3_4_1_resolve_keyword_patient_name() {
        let obj = create_test_obj();
        let spec = TagSpecifier::Keyword("PatientName".into());
        let tags = resolve_tags(&spec, &obj).expect("should resolve");
        assert_eq!(tags, vec![tags::PATIENT_NAME]);
    }

    // -- r-3-4-2 -------------------------------------------------------------

    /// Requirement r-3-4-2
    #[test]
    fn r3_4_2_parse_parenthesized_tag() {
        let tag = parse_parenthesized_tag("(0010,0020)").expect("should parse");
        assert_eq!(tag, Tag(0x0010, 0x0020));
    }

    /// Requirement r-3-4-2
    #[test]
    fn r3_4_2_parse_parenthesized_tag_uppercase() {
        let tag = parse_parenthesized_tag("(0010,0020)").expect("should parse");
        assert_eq!(tag, Tag(0x0010, 0x0020));
    }

    /// Requirement r-3-4-2
    #[test]
    fn r3_4_2_parse_bare_hex_tag() {
        let tag = parse_bare_hex_tag("00100020").expect("should parse");
        assert_eq!(tag, Tag(0x0010, 0x0020));
    }

    /// Requirement r-3-4-2
    #[test]
    fn r3_4_2_resolve_tag_value_specifier() {
        let obj = create_test_obj();
        let spec = TagSpecifier::TagValue(Tag(0x0010, 0x0020));
        let tags = resolve_tags(&spec, &obj).expect("should resolve");
        assert_eq!(tags, vec![Tag(0x0010, 0x0020)]);
    }

    // -- r-3-4-3 -------------------------------------------------------------

    /// Requirement r-3-4-3
    #[test]
    fn r3_4_3_resolve_private_tag() {
        let mut obj = create_test_obj();
        // Register a private creator block
        put_str(&mut obj, Tag(0x0009, 0x0010), VR::LO, "MY PRIVATE CREATOR");
        put_str(&mut obj, Tag(0x0009, 0x1001), VR::LO, "private value");

        let spec = TagSpecifier::PrivateTag {
            group: 0x0009,
            creator: "MY PRIVATE CREATOR".into(),
            element_offset: 0x01,
        };
        let tags = resolve_tags(&spec, &obj).expect("should resolve");
        // Should resolve to Tag(0x0009, 0x1001) since creator is at slot 0x10
        assert_eq!(tags, vec![Tag(0x0009, 0x1001)]);
    }

    // -- r-3-5 ---------------------------------------------------------------

    /// Requirement r-3-5
    #[test]
    fn r3_5_pattern_matches_multiple_tags() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "John^Doe");
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "12345");
        put_str(&mut obj, tags::PATIENT_SEX, VR::CS, "M");
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        // Pattern matching tags whose keyword starts with "Patient"
        let spec = TagSpecifier::Pattern("Patient.*".into());
        let matched = resolve_tags(&spec, &obj).expect("should resolve");
        assert!(matched.contains(&tags::PATIENT_NAME));
        assert!(matched.contains(&tags::PATIENT_ID));
        assert!(matched.contains(&tags::PATIENT_SEX));
        assert!(
            !matched.contains(&tags::MODALITY),
            "Modality should not match Patient.* pattern"
        );
    }

    /// Requirement r-3-5
    #[test]
    fn r3_5_pattern_matches_by_tag_value() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::PATIENT_NAME, VR::PN, "John^Doe");
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "12345");

        // Pattern matching tags by group 0010
        let spec = TagSpecifier::Pattern("(0010,.*)".into());
        let matched = resolve_tags(&spec, &obj).expect("should resolve");
        assert!(matched.contains(&tags::PATIENT_NAME));
        assert!(matched.contains(&tags::PATIENT_ID));
    }

    // -- r-3-4-4 -------------------------------------------------------------

    /// Build an object carrying overlay planes, curve groups, and a few
    /// ordinary tags that must survive.
    fn obj_with_repeating_groups() -> InMemDicomObject {
        let mut obj = create_test_obj();
        for group in [0x6000u16, 0x6002, 0x601E, 0x6080, 0x60FE] {
            put_str(&mut obj, Tag(group, 0x3000), VR::OW, "overlay-bitmap");
            put_str(&mut obj, Tag(group, 0x0022), VR::LO, "Dr Smith review");
            put_str(&mut obj, Tag(group, 0x0010), VR::US, "512");
        }
        for group in [0x5000u16, 0x5002, 0x50FE] {
            put_str(&mut obj, Tag(group, 0x3000), VR::OW, "curve-data");
            put_str(&mut obj, Tag(group, 0x200C), VR::OW, "audio-sample");
        }
        // Must not be touched: image Rows/Columns share the element
        // numbers 0010/0011 with OverlayRows/OverlayColumns.
        put_str(&mut obj, tags::ROWS, VR::US, "512");
        put_str(&mut obj, tags::COLUMNS, VR::US, "512");
        put_str(&mut obj, tags::PATIENT_ID, VR::LO, "MRN1");
        obj
    }

    /// Requirement r-3-4-4: a wildcard group matches every plane.
    #[test]
    fn r3_4_4_wildcard_group_matches_all_overlay_planes() {
        let obj = obj_with_repeating_groups();
        let spec = TagSpecifier::Wildcard {
            group: (0x6000, 0xFF00),
            element: (0x0000, 0x0000),
        };
        let matched = resolve_tags(&spec, &obj).expect("should resolve");
        assert_eq!(matched.len(), 15, "5 overlay groups x 3 elements");
        assert!(matched.iter().all(|t| (0x6000..=0x60FF).contains(&t.0)));
    }

    /// Requirement r-3-4-4: the image Rows/Columns tags share element
    /// numbers with OverlayRows/OverlayColumns and must never match.
    #[test]
    fn r3_4_4_wildcard_does_not_match_image_pixel_module() {
        let obj = obj_with_repeating_groups();
        for spec in [
            TagSpecifier::Wildcard {
                group: (0x6000, 0xFF00),
                element: (0x0000, 0x0000),
            },
            TagSpecifier::Wildcard {
                group: (0x5000, 0xFF00),
                element: (0x0000, 0x0000),
            },
        ] {
            let matched = resolve_tags(&spec, &obj).expect("should resolve");
            assert!(!matched.contains(&tags::ROWS), "must not match (0028,0010)");
            assert!(
                !matched.contains(&tags::COLUMNS),
                "must not match (0028,0011)"
            );
            assert!(!matched.contains(&tags::PATIENT_ID));
        }
    }

    /// Requirement r-3-4-4: a wildcard element targets one attribute
    /// across every plane.
    #[test]
    fn r3_4_4_wildcard_group_with_fixed_element() {
        let obj = obj_with_repeating_groups();
        let spec = TagSpecifier::Wildcard {
            group: (0x6000, 0xFF00),
            element: (0x3000, 0xFFFF),
        };
        let mut matched = resolve_tags(&spec, &obj).expect("should resolve");
        matched.sort();
        assert_eq!(
            matched,
            vec![
                Tag(0x6000, 0x3000),
                Tag(0x6002, 0x3000),
                Tag(0x601E, 0x3000),
                Tag(0x6080, 0x3000),
                Tag(0x60FE, 0x3000),
            ]
        );
    }

    /// Requirement r-3-4-4: a wildcard resolves only against tags the
    /// file actually carries, so an absent plane yields nothing.
    #[test]
    fn r3_4_4_wildcard_resolves_only_present_tags() {
        let obj = create_test_obj();
        let spec = TagSpecifier::Wildcard {
            group: (0x6000, 0xFF00),
            element: (0x0000, 0x0000),
        };
        assert!(
            resolve_tags(&spec, &obj)
                .expect("should resolve")
                .is_empty(),
            "no overlays present, nothing to remove"
        );
    }
}
