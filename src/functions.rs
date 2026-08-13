use crate::error::DeidError;
use crate::metadata::DeidFunction;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::sync::OnceLock;

/// Generate a DICOM UID from the SHA-256 hash of the input value.
///
/// The output has the form `2.25.<decimal>` where `<decimal>` is the
/// first 128 bits of the digest interpreted as a big-endian unsigned
/// integer. The result is truncated to 64 characters (the DICOM UID
/// maximum length).
///
/// When a salt is provided, it is prepended to the input before
/// hashing, so the digest is `SHA-256(salt + input)`. This exactly
/// matches the companion Python implementation:
///
/// ```python
/// digest = hashlib.sha256(f"{hash_salt}{value}".encode("utf-8")).digest()
/// return "2.25." + str(int.from_bytes(digest[:16], "big"))
/// ```
///
/// No salt (or an empty salt) is plain SHA-256 of the input,
/// byte-identical to historical output.
///
/// This is deterministic: the same input and salt always produce the
/// same UID.
fn hashuid(input: &str, salt: Option<&str>) -> Result<String, DeidError> {
    let mut hasher = Sha256::new();
    if let Some(salt) = salt {
        hasher.update(salt.as_bytes());
    }
    hasher.update(input.as_bytes());
    let digest = hasher.finalize();
    // Take the first 16 bytes (128 bits) as a u128
    let bytes: [u8; 16] = digest[..16].try_into().expect("slice is 16 bytes");
    let num = u128::from_be_bytes(bytes);
    let uid = format!("2.25.{}", num);
    // DICOM UIDs must be at most 64 characters
    Ok(uid[..uid.len().min(64)].to_string())
}

/// The Implementation Class UID identifying this tool.
///
/// PS3.15 E.1.1 step 7 requires the File Meta Information to be
/// replaced with "a description of the de-identifying application",
/// which includes the implementation information in (0002,0012) and
/// (0002,0013). This UID is derived deterministically from the package
/// name via [`hashuid`], so it needs no registered UID root and is
/// identical across every run.
///
/// The name deliberately excludes the version: (0002,0012) identifies
/// the *implementation*, while the version is carried separately in
/// (0002,0013) by [`implementation_version_name`].
pub fn implementation_class_uid() -> &'static str {
    static UID: OnceLock<String> = OnceLock::new();
    UID.get_or_init(|| {
        hashuid(env!("CARGO_PKG_NAME"), None).expect("hashuid over a fixed input cannot fail")
    })
}

/// The Implementation Version Name for (0002,0013).
///
/// (0002,0013) has VR `SH`, which is limited to 16 characters, so this
/// uses a shortened product name and is truncated defensively.
pub fn implementation_version_name() -> &'static str {
    static NAME: OnceLock<String> = OnceLock::new();
    NAME.get_or_init(|| {
        let mut name = format!("deid-rs {}", env!("CARGO_PKG_VERSION"));
        name.truncate(16);
        name
    })
}

/// Return the default built-in functions available in recipes.
///
/// If `salt` is provided, it is captured by the built-in `hashuid`
/// function and mixed into every hash it produces.
pub fn default_functions(salt: Option<&str>) -> HashMap<String, DeidFunction> {
    let salt = salt.map(str::to_string);
    let mut map: HashMap<String, DeidFunction> = HashMap::new();
    map.insert(
        "hashuid".into(),
        Box::new(move |input: &str| hashuid(input, salt.as_deref())),
    );
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashuid_deterministic() {
        let a = hashuid("1.2.840.113619.2.55.3.604688119.969.1068842234.928", None).unwrap();
        let b = hashuid("1.2.840.113619.2.55.3.604688119.969.1068842234.928", None).unwrap();
        assert_eq!(a, b, "same input should produce same output");
    }

    #[test]
    fn hashuid_different_inputs_different_outputs() {
        let a = hashuid("1.2.3.4.5", None).unwrap();
        let b = hashuid("1.2.3.4.6", None).unwrap();
        assert_ne!(a, b, "different inputs should produce different outputs");
    }

    #[test]
    fn hashuid_has_correct_prefix() {
        let uid = hashuid("1.2.3.4.5", None).unwrap();
        assert!(uid.starts_with("2.25."), "UID should start with 2.25.");
    }

    #[test]
    fn hashuid_max_64_chars() {
        let uid = hashuid("1.2.3.4.5", None).unwrap();
        assert!(
            uid.len() <= 64,
            "UID length {} exceeds DICOM max of 64",
            uid.len()
        );
    }

    #[test]
    fn hashuid_only_digits_and_dots() {
        let uid = hashuid("1.2.3.4.5", None).unwrap();
        assert!(
            uid.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "UID should only contain digits and dots, got: {}",
            uid
        );
    }

    #[test]
    fn hashuid_empty_input() {
        let uid = hashuid("", None).unwrap();
        assert!(uid.starts_with("2.25."));
        assert!(uid.len() <= 64);
    }

    /// Requirement r-3-6-1
    #[test]
    fn hashuid_salted_deterministic() {
        let a = hashuid("MRN-12345", Some("pepper")).unwrap();
        let b = hashuid("MRN-12345", Some("pepper")).unwrap();
        assert_eq!(a, b, "same input and salt should produce same output");
    }

    /// Requirement r-3-6-1
    #[test]
    fn hashuid_salt_changes_output() {
        let unsalted = hashuid("MRN-12345", None).unwrap();
        let salted = hashuid("MRN-12345", Some("pepper")).unwrap();
        assert_ne!(unsalted, salted, "salt should change the output");
    }

    /// Requirement r-3-6-1
    #[test]
    fn hashuid_different_salts_different_outputs() {
        let a = hashuid("MRN-12345", Some("salt-a")).unwrap();
        let b = hashuid("MRN-12345", Some("salt-b")).unwrap();
        assert_ne!(a, b, "different salts should produce different outputs");
    }

    /// Requirement r-3-6-1
    #[test]
    fn hashuid_salted_output_is_valid_uid() {
        let uid = hashuid("1.2.3.4.5", Some("pepper")).unwrap();
        assert!(uid.starts_with("2.25."), "UID should start with 2.25.");
        assert!(
            uid.len() <= 64,
            "UID length {} exceeds DICOM max of 64",
            uid.len()
        );
        assert!(
            uid.chars().all(|c| c.is_ascii_digit() || c == '.'),
            "UID should only contain digits and dots, got: {}",
            uid
        );
    }

    #[test]
    fn default_functions_contains_hashuid() {
        let funcs = default_functions(None);
        assert!(funcs.contains_key("hashuid"));
        let result = funcs["hashuid"]("1.2.3").unwrap();
        assert!(result.starts_with("2.25."));
    }

    /// Requirement r-3-6-1
    #[test]
    fn default_functions_hashuid_uses_salt() {
        let unsalted = default_functions(None)["hashuid"]("1.2.3").unwrap();
        let salted = default_functions(Some("pepper"))["hashuid"]("1.2.3").unwrap();
        assert_ne!(unsalted, salted, "registered hashuid should apply the salt");
        let salted_again = default_functions(Some("pepper"))["hashuid"]("1.2.3").unwrap();
        assert_eq!(
            salted, salted_again,
            "salted hashuid should be deterministic"
        );
    }

    /// Requirement r-3-6-1
    ///
    /// Known-answer vectors generated with the companion Python
    /// implementation:
    ///
    /// ```python
    /// digest = hashlib.sha256(f"{hash_salt}{value}".encode("utf-8")).digest()
    /// return "2.25." + str(int.from_bytes(digest[:16], "big"))
    /// ```
    #[test]
    fn hashuid_matches_python_reference_implementation() {
        // (salt, input, expected output from Python)
        let vectors = [
            (
                Some("pepper"),
                "MRN-12345",
                "2.25.236350546493157369760816461380098478256",
            ),
            (
                Some("my-secret-salt"),
                "1.2.840.113619.2.55.3.604688119.969.1068842234.928",
                "2.25.187718910066404343956371041083574392136",
            ),
            (
                Some("pepper"),
                "",
                "2.25.187067080696985313726142105206677996157",
            ),
            // Python with an empty salt string equals unsalted Rust output
            (
                None,
                "1.2.3.4.5",
                "2.25.245543443047141410495047436863840807046",
            ),
            (
                Some(""),
                "1.2.3.4.5",
                "2.25.245543443047141410495047436863840807046",
            ),
        ];
        for (salt, input, expected) in vectors {
            let uid = hashuid(input, salt).unwrap();
            assert_eq!(
                uid, expected,
                "mismatch with Python output for salt {:?}, input {:?}",
                salt, input
            );
        }
    }
}
