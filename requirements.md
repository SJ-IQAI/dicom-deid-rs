r-1 Inputs and Outputs
r-1-1 The software must accept a path to a directory of DICOM files as an input, a path to the output directory, and a path to the recipe file.
r-1-2 The software must recursively search the input directory for all DICOM files
r-1-3 The software must display a progress bar on the console as it processes input files
r-1-4 By default the software must preserve the relative directory structure of input files in the output directory (e.g., input/sub/file.dcm → output/sub/file.dcm). This is the behavior when no output layout (r-1-6) is configured.
r-1-5 The software must continue processing remaining files when an individual file fails, logging a warning and counting the file as skipped in the final report. The sole exception is the fatal condition defined in r-3-14-1, which aborts the run.
r-1-6 The software must support an optional output layout that names output files from de-identified tag values rather than mirroring the input tree, since input paths are conventionally named after the very identifiers being removed. The layout is a "/"-separated template whose {Token} placeholders name DICOM tags by keyword, parenthesized tag, or bare hex, e.g. "{PatientID}/{StudyInstanceUID}/{SeriesInstanceUID}_{SeriesNumber}/{SOPInstanceUID}.dcm". Placeholders must be resolved to tags when the pipeline is constructed, so an unknown or malformed template fails before any file is processed. A template containing no placeholders must be rejected. Values must be read from the data set after all de-identification actions and the file meta sync have been applied, so the path reflects the values actually written into the file.
r-1-7 Output layout path components must be safe by construction. Tag values must have DICOM padding trimmed, characters outside [A-Za-z0-9._-] replaced with "_" (consecutive replacements collapsing to one), trailing dots and spaces removed, Windows reserved base names escaped, and components exceeding 255 bytes rejected. A rendered path must never contain a path separator introduced by a tag value, nor a "." or ".." component, so that joining it onto the output directory cannot escape that directory. Literal text in a template is author-controlled and must be rejected rather than sanitized if it falls outside [A-Za-z0-9._-].
r-1-8 When a tag named by the output layout is absent, unreadable, or empty after trimming, the software must report the error and count the file as skipped per r-1-5, rather than writing to a degenerate path.
r-1-9 The software must detect when two input files render to the same output path and must not overwrite the earlier file. The later file must be reported and counted as skipped per r-1-5. Detection must hold across threads in the parallel runner, and claimed paths must be reset between runs of the same pipeline.
r-1-10 The software must support an optional mapping file recording each original input path alongside its de-identified output path, tab-separated with a header row. Because the mapping names the input files it is PHI by construction, so the software must refuse to start when the configured mapping path resolves inside the output directory. The parallel runner must produce the same mapping content as the sequential runner.
r-1-11 The blacklist report must be written to the current working directory, not the output directory. Blacklisted files are never de-identified, so the report necessarily names input paths; keeping it out of the output directory is what allows that directory to be free of PHI. The path actually written must be returned in the run report.
r-1-12 When an output layout is configured, the software must warn for each layout tag that no recipe header action de-identifies, since such a tag places its original value directly into the output path. Tags whose standard VR cannot carry identifying text, a UID, or a date (e.g. SeriesNumber, VR IS) must not trigger the warning. The condition is a warning rather than an error, since a site may supply already-pseudonymous identifiers.

r-2 De-id Recipe Specification
r-2-1 The software must parse a de-identification recipe file defining the deid operations to be performed
r-2-2 The recipe must begin with a FORMAT declaration line (e.g. FORMAT dicom). The parser must validate that the declared format is supported.
r-2-3 The recipe must support sections declared with a % prefix: %header for metadata de-id actions, and %filter <name> for named filter groups (e.g. %filter graylist, %filter blacklist).
r-2-4 Lines beginning with # must be treated as comments and ignored. Inline comments after # on action lines must also be stripped.
r-2-5 Under %filter sections, the software must parse LABEL directives that define named filter groups. Each group consists of a LABEL <name> line (with optional # comment), one or more filter condition lines, and zero or more coordinate directives.
r-2-6 Filter conditions
r-2-6-1 The software must support the filter predicate "contains <Field> <Value>" which checks if the field value contains the given substring or regex.
r-2-6-2 The software must support the filter predicate "notcontains <Field> <Value>".
r-2-6-3 The software must support the filter predicate "equals <Field> <Value>" which performs a case-insensitive exact match.
r-2-6-4 The software must support the filter predicate "notequals <Field> <Value>".
r-2-6-5 The software must support the filter predicate "missing <Field>" which checks that the field is not present in the DICOM.
r-2-6-6 The software must support the filter predicate "empty <Field>" which checks that the field is present but has an empty value.
r-2-6-7 The software must support the filter predicate "present <Field>" which checks that the field exists.
r-2-6-9 The software must support the filter predicate "blank <Field>" which is true when the field is absent *or* present with an empty value. This is the convention CTP filter scripts use for `Tag.equals("")`: a script cannot distinguish an absent element from a present-but-empty one, since both read as the empty string. It is deliberately distinct from "empty" (r-2-6-6), which requires the element to be present; converting a CTP `equals("")` test to "empty" would silently stop matching files that omit the tag altogether, and where such a test drives a rejection rule that failure is unsafe.
r-2-6-10 The software must support the filter predicate "notblank <Field>" which is true when the field is present and carries a non-empty value, the exact negation of r-2-6-9 and the equivalent of CTP's `!Tag.equals("")`. It must not be confused with "present" (r-2-6-7), which is also true for a present-but-empty element.
r-2-6-11 Filter predicate fields must support a `::` qualifier to address elements nested inside a sequence, e.g. `SequenceOfUltrasoundRegions::RegionDataType`, matching CTP's `Seq::Element` filter syntax. Resolution must search the named sequence's items in order and take the first item that carries the element. Qualifiers must nest, so `A::B::C` reaches two levels down. A field naming an absent sequence, a sequence with no items, or an element absent from every item must resolve as missing, which for the predicates above means a `::` field behaves exactly as a top-level field does when absent. Without this, an unresolvable field never matches, which silently disables every rule that depends on it.
r-2-6-8 Filter predicate semantics must not be altered by any internal indexing or dispatch optimisation: evaluating a label through the filter index must give the same result as evaluating it directly. In particular, `contains` must remain a regex match (r-2-6-1) and `equals` must remain an exact match (r-2-6-3) on every field, including Modality and Manufacturer, which the dispatch tree buckets by literal substring. A dispatch lookup may stand in for a condition only when it is exactly equivalent to evaluating that condition; otherwise it may narrow the candidate set but the condition must still be evaluated.
r-2-7 Logical operators in filter conditions
r-2-7-1 The software must support the + prefix on a filter line to indicate an AND relationship with the preceding condition.
r-2-7-2 The software must support the || prefix on a filter line to indicate an OR relationship with the preceding condition.
r-2-7-3 The software must support inline || and + within a single line to chain multiple conditions (e.g. "missing Manufacturer || empty Manufacturer").
r-2-7-4 The software must support pipe-separated alternatives within filter values (e.g. "contains ManufacturerModelName A400|A500"), which are treated as regex alternations.
r-2-8 Coordinate directives in filter groups
r-2-8-1 The software must support "coordinates x,y,xmax,ymax" to specify pixel regions to mask in (xmin, ymin, xmax, ymax) format.
r-2-8-2 The software must support "ctpcoordinates x,y,width,height" in CTP format, converting internally to (xmin, ymin, xmin+width, ymin+height).
r-2-8-3 The software must support "keepcoordinates" and "ctpkeepcoordinates" to specify regions to preserve (inverse mask).
r-2-8-4 A filter group may specify multiple coordinate regions.
r-2-9 Header action value types
r-2-9-1 Header action values must support literal string values (e.g. MODIFIED, YES).
r-2-9-2 Header action values must support variable references via var:<NAME> syntax (e.g. var:DATEINC). Variables must be provided at runtime by the caller.
r-2-9-3 Header action values must support function references via func:<name> syntax (e.g. func:hashuid).
r-2-10 Named filter types
r-2-10-1 The software must support "graylist" filters, which flag matching files and apply pixel masking based on the filter group's coordinate directives.
r-2-10-2 The software must support "blacklist" filters, which exclude matching files from the output entirely.
r-2-10-3 The software must support "allowlist" filters, which exempt a matching file from every blacklist rule (r-5-2). The exemption must suppress rejection and nothing else: an allowlisted file must still have graylist masking (r-2-10-1) and all header actions (r-3) applied to it. This is what makes an allowlist usable the way CTP device whitelists are used — they admit specific devices that are *known* to carry burned-in PHI, on the understanding that the pixel masking rules will remove it, so treating an allowlist match as "emit this file unmodified" would publish that PHI. A recipe declaring no allowlist section must exempt nothing, leaving blacklist behavior unchanged.

r-3 Metadata De-identification
r-3-1 The software must support adding a DICOM tag with a defined value
r-3-2 The software must support replacing a DICOM tag with a new value
r-3-3 The software must support deleting a DICOM tag entirely
r-3-4 Specifying DICOM tags
r-3-4-1 The software must support specifying a tag by its keyword (e.g. PatientId)
r-3-4-2 The software must support specifying a tag by its tag value in parenthesized format (e.g. (0002,0080)) or bare hex format (e.g. 00120063)
r-3-4-3 The software must support specifying private tags by its group, private creator, and element offset
r-3-4-4 The software must support specifying tags with `x` wildcard nibbles in either the parenthesized or bare hex form (e.g. `(60xx,xxxx)`, `60xxxxxx`), the notation DICOM PS3.6 and CTP use for repeating groups. Each nibble written as `x` matches any value; all other nibbles must match exactly. A specifier containing no wildcard nibble must continue to parse as an exact tag, and a malformed field must be a parse error rather than falling through to a keyword. Wildcards resolve against the tags a data set actually carries, so a group that is absent contributes nothing. This makes the repeating overlay (`6000-60FF`) and curve (`5000-50FF`) groups removable, matching CTP's `<r t="overlays">` and `<r t="curves">` directives. Because matching is on the full tag, the overlay elements `OverlayRows`/`OverlayColumns` `(60xx,0010)`/`(60xx,0011)` must never match the Image Pixel module's `Rows`/`Columns` `(0028,0010)`/`(0028,0011)`, which share their element numbers.
r-3-5 The software must support pattern matching of tags based on regexes of tag keywords or tag values, and applying deid operations to all tags matching the pattern
r-3-6 The software must support the use of pre-defined functions referenced via func:<name> syntax in the recipe to execute logic. Functions may accept keyword arguments.
r-3-6-1 The built-in hashuid function must support an optional caller-supplied salt (via configuration or the --salt CLI flag). When a salt is provided, the digest must be SHA-256 of the salt string prepended to the input value (SHA-256(salt + value)), remaining deterministic for the same input and salt. When no salt is provided, output must be identical to the unsalted (plain SHA-256) behavior, which equals the salted behavior with an empty salt string.
r-3-6-2 The software must provide a built-in hashuid_ascii function for identifier-style fields (e.g. PatientID, AccessionNumber) that returns the first 15 bytes of SHA-256(salt + value) encoded as lowercase unpadded RFC 4648 base32 (always 24 characters). It must honor the same optional caller-supplied salt as hashuid, remain deterministic for the same input and salt, and treat an empty salt string as no salt.
r-3-7 The software must support applying a "jitter" to date and datetime fields to shift the value by the specified number of days. DateTime (DT) fields must also be supported, preserving the time component while shifting only the date portion. Jittering a blank or empty date field must be a no-op (no error).
r-3-8 The software must support referencing variables within the recipe via var:<NAME> syntax to allow for dynamic values
r-3-9 The software must support blanking a DICOM tag (setting its value to empty/null) while keeping the tag present in the file
r-3-10 The software must support explicitly keeping a tag's original value unchanged, protecting it from removal by broader rules
r-3-11 When multiple actions apply to the same field, the software must respect a precedence hierarchy: KEEP > ADD > REPLACE > JITTER > REMOVE > BLANK
r-3-12 The software must support bulk removal of private tags from DICOM files
r-3-13 All metadata de-identification actions (ADD, REPLACE, REMOVE, BLANK, KEEP, JITTER) and private tag removal must apply recursively to elements nested within DICOM sequences (VR=SQ) at any depth.

r-3-14 The software must de-identify the File Meta Information group (group 0002) and keep it consistent with the de-identified data set. This must be applied unconditionally to every written file and must not be configurable or disableable from the recipe. Recipe header actions do not address group 0002 tags.
r-3-14-1 Media Storage SOP Instance UID (0002,0003) must always equal the de-identified data set's SOP Instance UID (0008,0018). If the data set has no non-empty (0008,0018) after de-identification, the software must report the error and abort the run rather than writing the file; this is the exception to r-1-5.
r-3-14-2 Media Storage SOP Class UID (0002,0002) must be set from the de-identified data set's SOP Class UID (0008,0016) when present, and left unchanged when the data set does not carry one.
r-3-14-3 The software must remove the identifying optional File Meta Information elements named by DICOM PS3.15 E.1.1: Source Application Entity Title (0002,0016), Sending Application Entity Title (0002,0017), Receiving Application Entity Title (0002,0018), Private Information Creator UID (0002,0100), and Private Information (0002,0102).
r-3-14-4 The software must set Implementation Class UID (0002,0012) and Implementation Version Name (0002,0013) to identify this de-identifying application. The Implementation Class UID must be deterministic across runs, and the Implementation Version Name must not exceed the 16-character limit of VR SH.
r-3-14-5 Transfer Syntax UID (0002,0010) must not be modified by the File Meta Information de-identification, since it governs the ability to read the file back. It remains under the control of pixel data decompression (r-4-8).

r-4 Pixel-based De-identification
r-4-1 The software must support pixel-based de-identification by masking over pixel areas
r-4-2 The software must support defining pixel areas to mask based on DICOM tags (e.g. overlay tags, burn-in tags, etc.)
r-4-3 The software must support both raw coordinate format (xmin, ymin, xmax, ymax) and CTP coordinate format (x, y, width, height), converting CTP coordinates internally
r-4-4 The software must support "keep" regions that are excluded from masking (inverse of mask regions)
r-4-5 A single filter group may define multiple coordinate regions to mask
r-4-6 Pixel masking regions must only be applied when the associated filter group's conditions match the DICOM file being processed

r-4-7 The software must support decompressing compressed pixel data before applying pixel masking. At minimum, the following transfer syntaxes must be supported: JPEG Baseline (1.2.840.10008.1.2.4.50), JPEG Lossless (1.2.840.10008.1.2.4.70), and RLE Lossless (1.2.840.10008.1.2.5). JPEG 2000 Lossless (1.2.840.10008.1.2.4.90) and JPEG 2000 (1.2.840.10008.1.2.4.91) must be supported when built with the jpeg2000 feature.
r-4-8 After decompressing and masking pixel data, the output must be stored as uncompressed pixel data and the transfer syntax updated to Explicit VR Little Endian (1.2.840.10008.1.2.1).

r-5 File Filtering
r-5-1 The software must support excluding DICOM files from processing entirely based on %filter blacklist rules. Files matching blacklist criteria must not appear in the output.
r-5-2 A file matching any %filter allowlist rule must be exempt from blacklist exclusion (r-2-10-3) and must appear in the output, de-identified. Allowlist evaluation must therefore precede blacklist evaluation. This ordering is what lets a recipe express a broad rejection rule alongside narrow exceptions to it — the structure every CTP institutional filter script uses, where a whitelist of validated devices is admitted through an otherwise blanket rejection of their modality or image type.

r-6 Embeddability
r-6-1 The software must be designed as a library, with the main rust entrypoint being a command-line interface to the library.
r-6-2 The software must provide an API for supplying additional functions and variables that can be referenced within the recipe file, allowing for extensibility and custom logic to be injected at runtime.
