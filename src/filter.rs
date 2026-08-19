use crate::recipe::{
    Condition, CoordinateRegion, FilterLabel, FilterType, LogicalOp, Predicate, Recipe,
};
use dicom_core::value::PrimitiveValue;
use dicom_object::InMemDicomObject;
use regex::Regex;

/// Resolve a DICOM field value as a string, returning None if the field is missing.
///
/// A field may be qualified with `::` to reach inside a sequence (r-2-6-11),
/// e.g. `SequenceOfUltrasoundRegions::RegionDataType`. The named sequence's
/// items are searched in order and the first item carrying the element wins,
/// which is how CTP's `Seq::Element` filter syntax resolves. Qualifiers nest,
/// so `A::B::C` reaches two levels down.
pub fn get_field_string(obj: &InMemDicomObject, field: &str) -> Option<String> {
    match field.split_once("::") {
        Some((seq, rest)) => {
            let items = obj.element_by_name(seq.trim()).ok()?.items()?;
            items
                .iter()
                .find_map(|item| get_field_string(item, rest.trim()))
        }
        None => obj
            .element_by_name(field)
            .ok()
            .and_then(|elem| elem.value().to_str().ok().map(|s| s.to_string())),
    }
}

/// Whether a field is absent or carries an empty value.
///
/// This is CTP's convention for `Tag.equals("")`: a filter script cannot
/// distinguish an absent element from a present-but-empty one, both read as
/// the empty string. It is deliberately *not* how the `empty` predicate
/// behaves (r-2-6-6), which requires the element to be present.
pub(crate) fn field_is_blank(obj: &InMemDicomObject, field: &str) -> bool {
    get_field_string(obj, field).is_none_or(|value| value.is_empty())
}

/// Evaluate a single filter predicate against a DICOM object.
///
/// Field names in predicates are resolved to DICOM tags by keyword lookup.
pub fn evaluate_predicate(predicate: &Predicate, obj: &InMemDicomObject) -> bool {
    match predicate {
        Predicate::Contains { field, value } => {
            let Some(field_val) = get_field_string(obj, field) else {
                return false;
            };
            let pattern = format!("(?i){}", value);
            match Regex::new(&pattern) {
                Ok(re) => re.is_match(&field_val),
                Err(_) => field_val.to_lowercase().contains(&value.to_lowercase()),
            }
        }
        Predicate::NotContains { field, value } => {
            let Some(field_val) = get_field_string(obj, field) else {
                return true;
            };
            let pattern = format!("(?i){}", value);
            match Regex::new(&pattern) {
                Ok(re) => !re.is_match(&field_val),
                Err(_) => !field_val.to_lowercase().contains(&value.to_lowercase()),
            }
        }
        Predicate::Equals { field, value } => {
            let Some(field_val) = get_field_string(obj, field) else {
                return false;
            };
            field_val.to_lowercase() == value.to_lowercase()
        }
        Predicate::NotEquals { field, value } => {
            let Some(field_val) = get_field_string(obj, field) else {
                return true;
            };
            field_val.to_lowercase() != value.to_lowercase()
        }
        Predicate::Missing { field } => obj.element_by_name(field).is_err(),
        Predicate::Empty { field } => match obj.element_by_name(field) {
            Ok(elem) => match elem.value() {
                dicom_core::value::Value::Primitive(prim) => match prim {
                    PrimitiveValue::Empty => true,
                    _ => elem.value().to_str().map(|s| s.is_empty()).unwrap_or(true),
                },
                _ => false,
            },
            Err(_) => false,
        },
        Predicate::Present { field } => obj.element_by_name(field).is_ok(),
        Predicate::Blank { field } => field_is_blank(obj, field),
        Predicate::NotBlank { field } => !field_is_blank(obj, field),
    }
}

/// Evaluate a list of conditions with logical operators against a DICOM object.
///
/// Conditions are evaluated left-to-right: each AND/OR operator combines the
/// running result with the current condition's result.
pub fn evaluate_conditions(conditions: &[Condition], obj: &InMemDicomObject) -> bool {
    let mut result = false;
    for condition in conditions {
        // Short-circuit: skip evaluation when result is already determined
        match condition.operator {
            LogicalOp::And if !result => continue,
            LogicalOp::Or if result => continue,
            _ => {}
        }
        let pred_result = evaluate_predicate(&condition.predicate, obj);
        result = match condition.operator {
            LogicalOp::First => pred_result,
            LogicalOp::And => result && pred_result,
            LogicalOp::Or => result || pred_result,
        };
    }
    result
}

/// Check whether a filter label's conditions match the given DICOM object.
pub fn matches_label(label: &FilterLabel, obj: &InMemDicomObject) -> bool {
    evaluate_conditions(&label.conditions, obj)
}

/// Check if a DICOM object is blacklisted by any blacklist filter in the recipe.
///
/// Returns `true` if the object matches any label within any blacklist filter
/// section, meaning it should be excluded from output.
pub fn is_blacklisted(recipe: &Recipe, obj: &InMemDicomObject) -> bool {
    blacklist_reason(recipe, obj).is_some()
}

/// Return the name of the first matching allowlist label, or `None` (r-2-10-3).
///
/// An allowlist match exempts the object from every blacklist rule and nothing
/// else: graylist masking and header actions still apply. A recipe with no
/// allowlist section exempts nothing, so blacklist behavior is unchanged for
/// recipes written before allowlists existed.
pub fn allowlist_exemption<'a>(recipe: &'a Recipe, obj: &InMemDicomObject) -> Option<&'a str> {
    recipe
        .filters
        .iter()
        .filter(|section| section.filter_type == FilterType::Allowlist)
        .flat_map(|section| &section.labels)
        .find(|label| matches_label(label, obj))
        .map(|label| label.name.as_str())
}

/// Whether any allowlist filter exempts this object from the blacklist.
pub fn is_allowlisted(recipe: &Recipe, obj: &InMemDicomObject) -> bool {
    allowlist_exemption(recipe, obj).is_some()
}

/// Return the name of the first matching blacklist label, or `None` if no
/// blacklist filter matches.
pub fn blacklist_reason<'a>(recipe: &'a Recipe, obj: &InMemDicomObject) -> Option<&'a str> {
    for section in &recipe.filters {
        if section.filter_type != FilterType::Blacklist {
            continue;
        }
        for label in &section.labels {
            if matches_label(label, obj) {
                return Some(&label.name);
            }
        }
    }
    None
}

/// Collect all coordinate regions from graylist filters whose conditions match
/// the given DICOM object.
pub fn get_graylist_regions(recipe: &Recipe, obj: &InMemDicomObject) -> Vec<CoordinateRegion> {
    let mut regions = Vec::new();
    for section in &recipe.filters {
        if section.filter_type != FilterType::Graylist {
            continue;
        }
        for label in &section.labels {
            if matches_label(label, obj) {
                regions.extend(label.coordinates.clone());
            }
        }
    }
    regions
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::recipe::*;
    use crate::test_helpers::*;
    use dicom_core::VR;
    use dicom_dictionary_std::tags;

    // -----------------------------------------------------------------------
    // Predicate evaluation (r-2-6)
    // -----------------------------------------------------------------------

    /// Requirement r-2-6-1
    #[test]
    fn r2_6_1_contains_matches_substring() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE MEDICAL SYSTEMS");

        let pred = Predicate::Contains {
            field: "Manufacturer".into(),
            value: "GE".into(),
        };
        assert!(evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-6-1
    #[test]
    fn r2_6_1_contains_no_match() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE MEDICAL SYSTEMS");

        let pred = Predicate::Contains {
            field: "Manufacturer".into(),
            value: "SIEMENS".into(),
        };
        assert!(!evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-6-1
    #[test]
    fn r2_6_1_contains_matches_regex() {
        let mut obj = create_test_obj();
        put_str(
            &mut obj,
            tags::MANUFACTURER_MODEL_NAME,
            VR::LO,
            "LightSpeed VCT",
        );

        let pred = Predicate::Contains {
            field: "ManufacturerModelName".into(),
            value: "Light.*VCT".into(),
        };
        assert!(evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-6-2
    #[test]
    fn r2_6_2_notcontains_rejects_substring() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE MEDICAL SYSTEMS");

        let pred = Predicate::NotContains {
            field: "Manufacturer".into(),
            value: "GE".into(),
        };
        assert!(
            !evaluate_predicate(&pred, &obj),
            "notcontains should be false when substring is present"
        );
    }

    /// Requirement r-2-6-2
    #[test]
    fn r2_6_2_notcontains_accepts_absent_substring() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE MEDICAL SYSTEMS");

        let pred = Predicate::NotContains {
            field: "Manufacturer".into(),
            value: "SIEMENS".into(),
        };
        assert!(evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-6-3
    #[test]
    fn r2_6_3_equals_case_insensitive_match() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let pred = Predicate::Equals {
            field: "Modality".into(),
            value: "ct".into(),
        };
        assert!(
            evaluate_predicate(&pred, &obj),
            "equals should be case-insensitive"
        );
    }

    /// Requirement r-2-6-3
    #[test]
    fn r2_6_3_equals_no_match() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let pred = Predicate::Equals {
            field: "Modality".into(),
            value: "MR".into(),
        };
        assert!(!evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-6-4
    #[test]
    fn r2_6_4_notequals_different_value() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let pred = Predicate::NotEquals {
            field: "Modality".into(),
            value: "MR".into(),
        };
        assert!(evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-6-4
    #[test]
    fn r2_6_4_notequals_same_value_case_insensitive() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let pred = Predicate::NotEquals {
            field: "Modality".into(),
            value: "ct".into(),
        };
        assert!(
            !evaluate_predicate(&pred, &obj),
            "notequals should be case-insensitive"
        );
    }

    /// Requirement r-2-6-5
    #[test]
    fn r2_6_5_missing_field_not_present() {
        let obj = create_test_obj();

        let pred = Predicate::Missing {
            field: "Manufacturer".into(),
        };
        assert!(
            evaluate_predicate(&pred, &obj),
            "missing should return true when field is absent"
        );
    }

    /// Requirement r-2-6-5
    #[test]
    fn r2_6_5_missing_field_is_present() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE");

        let pred = Predicate::Missing {
            field: "Manufacturer".into(),
        };
        assert!(
            !evaluate_predicate(&pred, &obj),
            "missing should return false when field is present"
        );
    }

    /// Requirement r-2-6-6
    #[test]
    fn r2_6_6_empty_field_present_and_empty() {
        let mut obj = create_test_obj();
        put_empty(&mut obj, tags::MANUFACTURER, VR::LO);

        let pred = Predicate::Empty {
            field: "Manufacturer".into(),
        };
        assert!(
            evaluate_predicate(&pred, &obj),
            "empty should return true when field is present but empty"
        );
    }

    /// Requirement r-2-6-6
    #[test]
    fn r2_6_6_empty_field_present_nonempty() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE");

        let pred = Predicate::Empty {
            field: "Manufacturer".into(),
        };
        assert!(
            !evaluate_predicate(&pred, &obj),
            "empty should return false when field has a value"
        );
    }

    /// Requirement r-2-6-6
    #[test]
    fn r2_6_6_empty_field_missing_entirely() {
        let obj = create_test_obj();

        let pred = Predicate::Empty {
            field: "Manufacturer".into(),
        };
        assert!(
            !evaluate_predicate(&pred, &obj),
            "empty should return false when field is not present at all"
        );
    }

    /// Requirement r-2-6-7
    #[test]
    fn r2_6_7_present_field_exists() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE");

        let pred = Predicate::Present {
            field: "Manufacturer".into(),
        };
        assert!(evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-6-7
    #[test]
    fn r2_6_7_present_field_absent() {
        let obj = create_test_obj();

        let pred = Predicate::Present {
            field: "Manufacturer".into(),
        };
        assert!(!evaluate_predicate(&pred, &obj));
    }

    // -----------------------------------------------------------------------
    // Logical operators (r-2-7)
    // -----------------------------------------------------------------------

    /// Requirement r-2-7-1
    #[test]
    fn r2_7_1_and_both_true() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::PATIENT_SEX, VR::CS, "M");
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let conditions = vec![
            Condition {
                operator: LogicalOp::First,
                predicate: Predicate::Equals {
                    field: "PatientSex".into(),
                    value: "M".into(),
                },
            },
            Condition {
                operator: LogicalOp::And,
                predicate: Predicate::Equals {
                    field: "Modality".into(),
                    value: "CT".into(),
                },
            },
        ];
        assert!(evaluate_conditions(&conditions, &obj));
    }

    /// Requirement r-2-7-1
    #[test]
    fn r2_7_1_and_second_false() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::PATIENT_SEX, VR::CS, "M");
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let conditions = vec![
            Condition {
                operator: LogicalOp::First,
                predicate: Predicate::Equals {
                    field: "PatientSex".into(),
                    value: "M".into(),
                },
            },
            Condition {
                operator: LogicalOp::And,
                predicate: Predicate::Equals {
                    field: "Modality".into(),
                    value: "MR".into(),
                },
            },
        ];
        assert!(!evaluate_conditions(&conditions, &obj));
    }

    /// Requirement r-2-7-2
    #[test]
    fn r2_7_2_or_first_true() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let conditions = vec![
            Condition {
                operator: LogicalOp::First,
                predicate: Predicate::Equals {
                    field: "Modality".into(),
                    value: "CT".into(),
                },
            },
            Condition {
                operator: LogicalOp::Or,
                predicate: Predicate::Equals {
                    field: "Modality".into(),
                    value: "MR".into(),
                },
            },
        ];
        assert!(evaluate_conditions(&conditions, &obj));
    }

    /// Requirement r-2-7-2
    #[test]
    fn r2_7_2_or_both_false() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "US");

        let conditions = vec![
            Condition {
                operator: LogicalOp::First,
                predicate: Predicate::Equals {
                    field: "Modality".into(),
                    value: "CT".into(),
                },
            },
            Condition {
                operator: LogicalOp::Or,
                predicate: Predicate::Equals {
                    field: "Modality".into(),
                    value: "MR".into(),
                },
            },
        ];
        assert!(!evaluate_conditions(&conditions, &obj));
    }

    /// Requirement r-2-7-3
    #[test]
    fn r2_7_3_mixed_and_or_operators() {
        // Evaluates left-to-right:
        //   (PatientSex==M AND Modality==CT) OR Manufacturer contains GE
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::PATIENT_SEX, VR::CS, "F");
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE MEDICAL");

        let conditions = vec![
            Condition {
                operator: LogicalOp::First,
                predicate: Predicate::Equals {
                    field: "PatientSex".into(),
                    value: "M".into(),
                },
            },
            Condition {
                operator: LogicalOp::And,
                predicate: Predicate::Equals {
                    field: "Modality".into(),
                    value: "CT".into(),
                },
            },
            Condition {
                operator: LogicalOp::Or,
                predicate: Predicate::Contains {
                    field: "Manufacturer".into(),
                    value: "GE".into(),
                },
            },
        ];
        // (F==M && CT==CT) || GE in "GE MEDICAL" => (false && true) || true => true
        assert!(evaluate_conditions(&conditions, &obj));
    }

    /// Requirement r-2-7-4
    #[test]
    fn r2_7_4_pipe_alternatives_match_first() {
        let mut obj = create_test_obj();
        put_str(
            &mut obj,
            tags::MANUFACTURER_MODEL_NAME,
            VR::LO,
            "A400 Scanner",
        );

        let pred = Predicate::Contains {
            field: "ManufacturerModelName".into(),
            value: "A400|A500|A600".into(),
        };
        assert!(
            evaluate_predicate(&pred, &obj),
            "pipe-separated value should match as regex alternation"
        );
    }

    /// Requirement r-2-7-4
    #[test]
    fn r2_7_4_pipe_alternatives_match_second() {
        let mut obj = create_test_obj();
        put_str(
            &mut obj,
            tags::MANUFACTURER_MODEL_NAME,
            VR::LO,
            "A500 Premium",
        );

        let pred = Predicate::Contains {
            field: "ManufacturerModelName".into(),
            value: "A400|A500|A600".into(),
        };
        assert!(evaluate_predicate(&pred, &obj));
    }

    /// Requirement r-2-7-4
    #[test]
    fn r2_7_4_pipe_alternatives_no_match() {
        let mut obj = create_test_obj();
        put_str(
            &mut obj,
            tags::MANUFACTURER_MODEL_NAME,
            VR::LO,
            "B700 Scanner",
        );

        let pred = Predicate::Contains {
            field: "ManufacturerModelName".into(),
            value: "A400|A500|A600".into(),
        };
        assert!(!evaluate_predicate(&pred, &obj));
    }

    // -----------------------------------------------------------------------
    // Blacklist / file filtering (r-5)
    // -----------------------------------------------------------------------

    /// Requirement r-5-1
    #[test]
    fn r5_1_blacklist_excludes_matching_file() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "SR");

        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![],
            filters: vec![FilterSection {
                filter_type: FilterType::Blacklist,
                labels: vec![FilterLabel {
                    name: "Reject Structured Reports".into(),
                    conditions: vec![Condition {
                        operator: LogicalOp::First,
                        predicate: Predicate::Equals {
                            field: "Modality".into(),
                            value: "SR".into(),
                        },
                    }],
                    coordinates: vec![],
                }],
            }],
        };

        assert!(
            is_blacklisted(&recipe, &obj),
            "SR modality should be blacklisted"
        );
    }

    /// Requirement r-5-1
    #[test]
    fn r5_1_blacklist_does_not_exclude_non_matching() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "CT");

        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![],
            filters: vec![FilterSection {
                filter_type: FilterType::Blacklist,
                labels: vec![FilterLabel {
                    name: "Reject Structured Reports".into(),
                    conditions: vec![Condition {
                        operator: LogicalOp::First,
                        predicate: Predicate::Equals {
                            field: "Modality".into(),
                            value: "SR".into(),
                        },
                    }],
                    coordinates: vec![],
                }],
            }],
        };

        assert!(
            !is_blacklisted(&recipe, &obj),
            "CT modality should not be blacklisted"
        );
    }

    /// Requirement r-5-1
    #[test]
    fn r5_1_graylist_does_not_exclude_file() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "SR");

        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![],
            filters: vec![FilterSection {
                filter_type: FilterType::Graylist, // graylist, not blacklist
                labels: vec![FilterLabel {
                    name: "Graylist SR".into(),
                    conditions: vec![Condition {
                        operator: LogicalOp::First,
                        predicate: Predicate::Equals {
                            field: "Modality".into(),
                            value: "SR".into(),
                        },
                    }],
                    coordinates: vec![],
                }],
            }],
        };

        assert!(
            !is_blacklisted(&recipe, &obj),
            "graylist filters should not cause blacklist exclusion"
        );
    }

    // -----------------------------------------------------------------------
    // Graylist region collection (r-2-10-1)
    // -----------------------------------------------------------------------

    /// Requirement r-2-10-1
    #[test]
    fn r2_10_1_graylist_returns_regions_on_match() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "GE MEDICAL SYSTEMS");

        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![],
            filters: vec![FilterSection {
                filter_type: FilterType::Graylist,
                labels: vec![FilterLabel {
                    name: "GE CT".into(),
                    conditions: vec![Condition {
                        operator: LogicalOp::First,
                        predicate: Predicate::Contains {
                            field: "Manufacturer".into(),
                            value: "GE".into(),
                        },
                    }],
                    coordinates: vec![CoordinateRegion {
                        xmin: 0,
                        ymin: 0,
                        xmax: 512,
                        ymax: 100,
                        keep: false,
                    }],
                }],
            }],
        };

        let regions = get_graylist_regions(&recipe, &obj);
        assert_eq!(regions.len(), 1);
        assert_eq!(regions[0].xmax, 512);
        assert_eq!(regions[0].ymax, 100);
    }

    /// Requirement r-2-10-1
    #[test]
    fn r2_10_1_graylist_no_regions_on_mismatch() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, "SIEMENS");

        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![],
            filters: vec![FilterSection {
                filter_type: FilterType::Graylist,
                labels: vec![FilterLabel {
                    name: "GE CT".into(),
                    conditions: vec![Condition {
                        operator: LogicalOp::First,
                        predicate: Predicate::Contains {
                            field: "Manufacturer".into(),
                            value: "GE".into(),
                        },
                    }],
                    coordinates: vec![CoordinateRegion {
                        xmin: 0,
                        ymin: 0,
                        xmax: 512,
                        ymax: 100,
                        keep: false,
                    }],
                }],
            }],
        };

        let regions = get_graylist_regions(&recipe, &obj);
        assert!(
            regions.is_empty(),
            "non-matching filters should yield no regions"
        );
    }

    // -----------------------------------------------------------------------
    // blank / notblank (r-2-6-9, r-2-6-10)
    // -----------------------------------------------------------------------

    /// Requirement r-2-6-9
    #[test]
    fn r2_6_9_blank_field_missing_entirely() {
        let obj = create_test_obj();

        let pred = Predicate::Blank {
            field: "SoftwareVersions".into(),
        };
        assert!(
            evaluate_predicate(&pred, &obj),
            "blank must be true for an absent field, unlike empty (r-2-6-6)"
        );
    }

    /// Requirement r-2-6-9
    #[test]
    fn r2_6_9_blank_field_present_and_empty() {
        let mut obj = create_test_obj();
        put_empty(&mut obj, tags::SOFTWARE_VERSIONS, VR::LO);

        let pred = Predicate::Blank {
            field: "SoftwareVersions".into(),
        };
        assert!(
            evaluate_predicate(&pred, &obj),
            "blank must be true for a present but empty field"
        );
    }

    /// Requirement r-2-6-9
    #[test]
    fn r2_6_9_blank_field_present_nonempty() {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::SOFTWARE_VERSIONS, VR::LO, "V6.06ER011");

        let pred = Predicate::Blank {
            field: "SoftwareVersions".into(),
        };
        assert!(
            !evaluate_predicate(&pred, &obj),
            "blank must be false when the field carries a value"
        );
    }

    /// Requirement r-2-6-10: notblank is the exact negation of blank.
    #[test]
    fn r2_6_10_notblank_is_negation_of_blank() {
        let missing = create_test_obj();

        let mut empty = create_test_obj();
        put_empty(&mut empty, tags::SOFTWARE_VERSIONS, VR::LO);

        let mut populated = create_test_obj();
        put_str(&mut populated, tags::SOFTWARE_VERSIONS, VR::LO, "VB60A");

        let blank = Predicate::Blank {
            field: "SoftwareVersions".into(),
        };
        let notblank = Predicate::NotBlank {
            field: "SoftwareVersions".into(),
        };

        for (obj, label) in [
            (&missing, "missing"),
            (&empty, "present but empty"),
            (&populated, "populated"),
        ] {
            assert_eq!(
                evaluate_predicate(&notblank, obj),
                !evaluate_predicate(&blank, obj),
                "notblank must be the negation of blank ({label})"
            );
        }

        assert!(
            evaluate_predicate(&notblank, &populated),
            "notblank must be true only for a present, non-empty field"
        );
    }

    /// Requirement r-2-6-10: notblank is stricter than present, which is also
    /// true for a present-but-empty element.
    #[test]
    fn r2_6_10_notblank_differs_from_present() {
        let mut obj = create_test_obj();
        put_empty(&mut obj, tags::PIXEL_SPACING, VR::DS);

        assert!(
            evaluate_predicate(
                &Predicate::Present {
                    field: "PixelSpacing".into()
                },
                &obj
            ),
            "present must be true for a present but empty field"
        );
        assert!(
            !evaluate_predicate(
                &Predicate::NotBlank {
                    field: "PixelSpacing".into()
                },
                &obj
            ),
            "notblank must be false for a present but empty field"
        );
    }

    // -----------------------------------------------------------------------
    // Sequence-qualified fields (r-2-6-11)
    // -----------------------------------------------------------------------

    fn us_regions_object(region_data_types: &[u16]) -> InMemDicomObject {
        let mut obj = create_test_obj();
        let items = region_data_types
            .iter()
            .map(|value| {
                let mut item = create_test_obj();
                put_u16(&mut item, tags::REGION_DATA_TYPE, VR::US, *value);
                item
            })
            .collect();
        put_sequence(&mut obj, tags::SEQUENCE_OF_ULTRASOUND_REGIONS, items);
        obj
    }

    /// Requirement r-2-6-11
    #[test]
    fn r2_6_11_sequence_qualified_field_resolves() {
        let obj = us_regions_object(&[3]);

        assert_eq!(
            get_field_string(&obj, "SequenceOfUltrasoundRegions::RegionDataType").as_deref(),
            Some("3"),
            "a :: qualified field must resolve from the sequence's item"
        );
    }

    /// Requirement r-2-6-11: the first item carrying the element wins.
    #[test]
    fn r2_6_11_sequence_qualified_field_takes_first_item_with_element() {
        let mut obj = create_test_obj();
        let bare = create_test_obj();
        let mut second = create_test_obj();
        put_u16(&mut second, tags::REGION_DATA_TYPE, VR::US, 4);
        put_sequence(
            &mut obj,
            tags::SEQUENCE_OF_ULTRASOUND_REGIONS,
            vec![bare, second],
        );

        assert_eq!(
            get_field_string(&obj, "SequenceOfUltrasoundRegions::RegionDataType").as_deref(),
            Some("4"),
            "items must be searched in order, skipping those without the element"
        );
    }

    /// Requirement r-2-6-11: an unresolvable qualified field behaves exactly as
    /// an absent top-level field does.
    #[test]
    fn r2_6_11_sequence_qualified_field_unresolvable_is_missing() {
        let field = "SequenceOfUltrasoundRegions::RegionDataType";

        let no_sequence = create_test_obj();
        let no_items = us_regions_object(&[]);
        let mut item_without_element = create_test_obj();
        put_sequence(
            &mut item_without_element,
            tags::SEQUENCE_OF_ULTRASOUND_REGIONS,
            vec![create_test_obj()],
        );

        for (obj, label) in [
            (&no_sequence, "sequence absent"),
            (&no_items, "sequence with no items"),
            (&item_without_element, "element absent from every item"),
        ] {
            assert_eq!(
                get_field_string(obj, field),
                None,
                "{label} must resolve as missing"
            );
            assert!(
                evaluate_predicate(
                    &Predicate::Blank {
                        field: field.into()
                    },
                    obj
                ),
                "{label} must read as blank"
            );
            assert!(
                !evaluate_predicate(
                    &Predicate::NotBlank {
                        field: field.into()
                    },
                    obj
                ),
                "{label} must not read as notblank"
            );
        }
    }

    /// Requirement r-2-6-11: `notblank Seq::Element` is the CTP screenshot test
    /// (`!SeqOfUltrasoundRegions::RegionDataType.equals("")`) — real ultrasound
    /// images carry the region sequence, report/screenshot pages do not.
    #[test]
    fn r2_6_11_notblank_sequence_field_distinguishes_screenshots() {
        let real_image = us_regions_object(&[3]);
        let screenshot = create_test_obj();

        let pred = Predicate::NotBlank {
            field: "SequenceOfUltrasoundRegions::RegionDataType".into(),
        };
        assert!(
            evaluate_predicate(&pred, &real_image),
            "an image with an ultrasound region sequence must pass"
        );
        assert!(
            !evaluate_predicate(&pred, &screenshot),
            "an image with no ultrasound region sequence must fail"
        );
    }

    /// Requirement r-2-6-11: qualifiers nest.
    #[test]
    fn r2_6_11_sequence_qualifier_nests_two_levels() {
        let mut inner = create_test_obj();
        put_str(&mut inner, tags::CODE_MEANING, VR::LO, "IEC Body Dosimetry");
        let mut middle = create_test_obj();
        put_sequence(&mut middle, tags::ANATOMIC_REGION_SEQUENCE, vec![inner]);
        let mut obj = create_test_obj();
        put_sequence(&mut obj, tags::SEQUENCE_OF_ULTRASOUND_REGIONS, vec![middle]);

        assert_eq!(
            get_field_string(
                &obj,
                "SequenceOfUltrasoundRegions::AnatomicRegionSequence::CodeMeaning"
            )
            .as_deref(),
            Some("IEC Body Dosimetry"),
            "a :: qualifier must nest to reach two levels down"
        );
    }

    // -----------------------------------------------------------------------
    // Allowlist (r-2-10-3, r-5-2)
    // -----------------------------------------------------------------------

    fn recipe_with_allowlist_and_blacklist() -> Recipe {
        Recipe {
            format: "dicom".into(),
            header: vec![],
            filters: vec![
                FilterSection {
                    filter_type: FilterType::Allowlist,
                    labels: vec![FilterLabel {
                        name: "Admit validated Aloka US".into(),
                        conditions: vec![
                            Condition {
                                operator: LogicalOp::First,
                                predicate: Predicate::Equals {
                                    field: "Modality".into(),
                                    value: "US".into(),
                                },
                            },
                            Condition {
                                operator: LogicalOp::And,
                                predicate: Predicate::Contains {
                                    field: "Manufacturer".into(),
                                    value: "Aloka".into(),
                                },
                            },
                        ],
                        coordinates: vec![],
                    }],
                },
                FilterSection {
                    filter_type: FilterType::Blacklist,
                    labels: vec![FilterLabel {
                        name: "Reject US".into(),
                        conditions: vec![Condition {
                            operator: LogicalOp::First,
                            predicate: Predicate::Contains {
                                field: "Modality".into(),
                                value: "US".into(),
                            },
                        }],
                        coordinates: vec![],
                    }],
                },
            ],
        }
    }

    fn us_object(manufacturer: &str) -> InMemDicomObject {
        let mut obj = create_test_obj();
        put_str(&mut obj, tags::MODALITY, VR::CS, "US");
        put_str(&mut obj, tags::MANUFACTURER, VR::LO, manufacturer);
        obj
    }

    /// Requirement r-2-10-3
    #[test]
    fn r2_10_3_allowlist_match_is_reported() {
        let recipe = recipe_with_allowlist_and_blacklist();

        assert_eq!(
            allowlist_exemption(&recipe, &us_object("Aloka")),
            Some("Admit validated Aloka US"),
            "a matching allowlist label must be reported by name"
        );
        assert_eq!(
            allowlist_exemption(&recipe, &us_object("ATL")),
            None,
            "a non-matching object must have no exemption"
        );
    }

    /// Requirement r-2-10-3: a recipe with no allowlist section exempts nothing,
    /// so blacklist behavior is unchanged for pre-existing recipes.
    #[test]
    fn r2_10_3_absent_allowlist_section_exempts_nothing() {
        let mut recipe = recipe_with_allowlist_and_blacklist();
        recipe
            .filters
            .retain(|section| section.filter_type != FilterType::Allowlist);

        assert!(
            !is_allowlisted(&recipe, &us_object("Aloka")),
            "with no allowlist section nothing may be exempt"
        );
        assert!(
            is_blacklisted(&recipe, &us_object("Aloka")),
            "the blacklist must still apply when no allowlist section exists"
        );
    }

    /// Requirement r-5-2: the allowlist exempts from the blacklist, and the
    /// blacklist itself is untouched — it still matches, it is just not
    /// consulted for an exempt file.
    #[test]
    fn r5_2_allowlist_exempts_from_blacklist() {
        let recipe = recipe_with_allowlist_and_blacklist();
        let aloka = us_object("Aloka");
        let atl = us_object("ATL");

        assert!(is_allowlisted(&recipe, &aloka));
        assert!(
            is_blacklisted(&recipe, &aloka),
            "the blacklist rule still matches; the exemption is applied by the caller"
        );

        assert!(!is_allowlisted(&recipe, &atl));
        assert!(
            is_blacklisted(&recipe, &atl),
            "an unlisted device of a rejected modality must stay blacklisted"
        );
    }
}
