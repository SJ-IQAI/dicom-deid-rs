# dicom-deid-rs

A DICOM de-identification tool written in Rust. Removes or masks protected
health information (PHI) from DICOM files based on a configurable recipe file.

This project is in very early stages of development-- consider it alpha software!

## Methods

This repository was primarily developed using Claude Code, leveraging two
extremely well-constructed de-identification libraries as references:

- [MIRC2][mirc2], otherwise known as RSNA's Clinical Trial Processor (CTP)
- [pydicom-deid][deid], an excellent DICOM de-identification library from the pydicom team.

[mirc2]: https://github.com/RSNA/MIRC2
[deid]: https://github.com/pydicom/deid/

While both of these libraries are tested and used widely, we found a need for a
very performance-focused implementation that can easily embed into web
deployment contexts. Thus, we adopted a spec-driven development approach where
we iterated on an initial set of requirements by manually drafting and
comparing with implementation details within these two repositories. Once we
achieved an acceptable level of feature parity, we then focused on performance
improvements.

Our many thanks to the dedicated authors of MIRC2, pydicom-deid, and the many
other open-source libraries whose work laid a solid foundation for this
project.

## Features

- **Metadata de-identification** -- add, replace, blank, remove, keep, or jitter DICOM tags
- **Pixel de-identification** -- mask burned-in PHI in pixel data based on tag-driven filter rules
- **Recipe-driven** -- all operations defined in a human-readable recipe file compatible with CTP conventions
- **Compressed pixel data** -- decompresses JPEG Baseline, JPEG Lossless, JPEG 2000, and RLE Lossless before masking
- **Blacklist filtering** -- exclude files from output entirely based on tag conditions
- **PatientID mapper** -- swap in pre-assigned study identifiers from a CSV or JSON file, changing nothing else
- **Embeddable** -- designed as a library with a CLI frontend; custom functions and variables can be injected at runtime

## Usage

```
dicom-deid-rs <input_dir> <output_dir> <recipe_file> [OPTIONS]
```

The tool recursively finds all `.dcm` files in `input_dir`, applies the recipe, and writes de-identified files to `output_dir`, preserving the directory structure by default.

```
dicom-deid-rs ./input ./output recipe.txt \
  --var PATIENT_ID "ANON-001" \
  --var PATIENT_NAME "Anonymous" \
  --var DATEINC "30" \
  --salt "my-secret-salt"
```

`--salt` mixes a secret into the built-in `hashuid` function by prepending it
to the value before hashing (`SHA-256(salt + value)`), so hashed values such
as `REPLACE PatientID func:hashuid` cannot be reversed by hashing candidate
inputs without the salt. Use the same salt across runs of a dataset to keep
hashed UIDs and IDs consistent; without `--salt`, output is plain (unsalted)
SHA-256, equivalent to an empty salt.

### PatientID mapper

When a site has already assigned study identifiers out of band, `--mapper`
supplies them:

```
dicom-deid-rs ./input ./output recipe.txt --mapper ./keys/ids.csv
```

`--mapper` overrides the recipe for **exactly one tag**. PatientID
(0010,0020) takes its value from the mapper; everything else the recipe does
— filters, pixel masking, every other header action, private tag removal —
runs exactly as it would without a mapper. Without `--mapper`, the recipe
governs PatientID as it always has.

The override is applied after the recipe's actions, so it wins whatever the
recipe did to PatientID, including a `REMOVE` — the mapper puts the tag back.
The original PatientID is looked up *before* the recipe runs, since the recipe
is what changes the value being looked up.

A file whose PatientID is missing, empty, or absent from the mapper is
reported and counted as skipped — never written — so no file is emitted
carrying an unmapped identifier.

CSV takes the original PatientID in the first column and the replacement in
the second. A header row naming the columns is recognized and skipped, and
when present the columns are located by name, so either column order works.
Names are matched ignoring case, spacing, and punctuation (`PatientID`,
`patient_id`, and `Patient ID` are one name); extra columns are ignored.

```csv
PatientID,DeidPatientID
MRN0012345,ANON-0001
MRN0067890,ANON-0002
```

JSON accepts an object of pairs, an array of `[original, replacement]` pairs,
or an array of objects keyed by those same column names:

```json
{ "MRN0012345": "ANON-0001", "MRN0067890": "ANON-0002" }
```

The file is read and validated before any DICOM file is processed, so a bad
mapper fails immediately rather than part way through a run. An empty
replacement, one longer than the 64 bytes VR LO allows, or one containing a
backslash or control character is rejected, as are two different replacements
for the same original — silently picking one would make the output depend on
row order. A repeated identical pair is fine, since mapper files are often
concatenated.

Lookup matches the value exactly once DICOM padding is trimmed. Nothing else
is normalized, so identifiers differing only in case are different patients.
PatientID nested inside sequences is mapped too, each occurrence by its own
value.

### De-identified output paths

Archives are conventionally laid out as
`<PatientID>/<StudyInstanceUID>/<SeriesInstanceUID>_<SeriesNumber>/<SOPInstanceUID>.dcm`,
so mirroring the input tree writes the original identifiers back into the
output as directory names — undoing the header work. `--deid-paths` names
output files from the *de-identified* values instead:

```
dicom-deid-rs ./input ./output recipe.txt \
  --salt "my-secret-salt" \
  --deid-paths \
  --mapping-file ./keys/mapping.tsv
```

```
input/MRN0012345/1.2.840.10.1/1.2.840.10.2_3/1.2.840.10.100.dcm
  ->
output/opcithikdfafnuugqkri27k2/2.25.2424779984772269505.../2.25.5448186484113305683..._3/2.25.8137828201831992686....dcm
```

The hierarchy is preserved: instances of a series still share a series
directory, and studies of a patient still share a patient directory. Values
are read from the data set *after* de-identification, so each path component
equals the value stored in the file.

`--output-layout TEMPLATE` takes a custom `/`-separated template whose
`{Token}` placeholders name DICOM tags by keyword, `(gggg,eeee)`, or bare
hex; `--deid-paths` is shorthand for the layout above. Unknown or malformed
templates fail before any file is processed.

Notes on using a layout:

- The paths are only PHI-free if the recipe actually de-identifies the tags
  the layout reads. Each layout tag no recipe action changes produces a
  startup warning.
- A file missing one of the layout tags is reported and counted as skipped;
  the run continues.
- Two inputs that render to the same path do not overwrite each other — the
  second is counted as skipped. This is the usual signal that a recipe blanks
  or removes an identifier the layout depends on.
- `--mapping-file` records `original_path <TAB> deidentified_path` for every
  written file. Hashing is one-way, so without it there is no way back from
  output to input. **This file lists the original paths and is therefore
  PHI**; it must sit outside `output_dir` (the tool refuses otherwise) and be
  stored accordingly.
- UID components run about 44 characters each, so a full de-identified path
  is roughly 170 characters plus your output root — worth checking against
  the 260-character `MAX_PATH` limit on Windows.

The blacklist report (`blacklisted_files.txt`) is written to the current
working directory, not to `output_dir`. Blacklisted files are never
de-identified, so the report necessarily names input paths; keeping it out of
the output directory is what lets that directory stay free of PHI.


## Recipe Format

Recipes begin with a `FORMAT dicom` declaration followed by `%filter` and `%header` sections.

```
FORMAT dicom

%filter blacklist

LABEL Scanned Documents
  contains ImageType Secondary
  contains Modality OT

%filter graylist

LABEL GE CT Dose Report
  contains Modality CT
  + contains Manufacturer GE
  + contains SeriesDescription Dose Report
  coordinates 0,0,512,110

%header

KEEP Modality YES
REPLACE PatientID var:PATIENT_ID
REPLACE PatientName var:PATIENT_NAME
REPLACE SOPInstanceUID func:hashuid
JITTER StudyDate var:DATEINC
BLANK PatientBirthDate YES
REMOVE InstitutionName YES
ADD PatientIdentityRemoved YES
```

### Filter predicates

`contains`, `notcontains`, `equals`, `notequals`, `missing`, `empty`, `present`

### Logical operators

- `+` (AND) and `||` (OR) between condition lines
- Pipe-separated alternatives in values (e.g. `contains Modality CT|MR`)

### Coordinate types

- `coordinates x,y,xmax,ymax` -- raw pixel region to mask
- `ctpcoordinates x,y,width,height` -- CTP format (converted internally)
- `keepcoordinates` / `ctpkeepcoordinates` -- regions to preserve

### Header actions

| Action    | Description                                        |
|-----------|----------------------------------------------------|
| `ADD`     | Add tag if not already present                     |
| `REPLACE` | Set tag value (creates if missing)                 |
| `REMOVE`  | Delete tag entirely                                |
| `BLANK`   | Clear value but keep tag present                   |
| `KEEP`    | Preserve original value (overrides other actions)  |
| `JITTER`  | Shift date/datetime by N days                      |

Precedence when multiple actions target the same tag: KEEP > ADD > REPLACE > JITTER > REMOVE > BLANK

### Value types

- Literal: `REPLACE StudyID ANONYMIZED`
- Variable: `REPLACE PatientID var:PATIENT_ID`
- Function: `REPLACE SOPInstanceUID func:hashuid`

### Tag formats

- Keyword: `PatientName`
- Bare hex: `00120063`
- Parenthesized: `(0008,0050)`
- Wildcard: `(60xx,xxxx)` or `60xxxxxx` — each `x` matches any nibble

Prefer a tag number over a keyword when transcribing from another tool's
config. Keywords are resolved against the DICOM dictionary, and an
unrecognised one fails at runtime rather than at parse time, which shows up
as every file being skipped.

### Repeating groups

The `x` wildcard is the notation DICOM PS3.6 and CTP use for repeating
groups. Its main use is removing overlay and curve planes, which CTP handles
with its `<r t="overlays">` and `<r t="curves">` directives:

```
REMOVE (60xx,xxxx)   # all overlay groups, 6000-60FF
REMOVE (50xx,xxxx)   # all curve groups, 5000-50FF
```

This matters for de-identification because overlay planes are a common
hiding place for burned-in annotation, and curve groups can carry audio.
Wildcards resolve against the tags a file actually carries, so absent groups
cost nothing.

Matching is on the whole tag, so `(60xx,xxxx)` does **not** touch the Image
Pixel module — `(0028,0010)` Rows and `(0028,0011)` Columns are unaffected
even though `OverlayRows`/`OverlayColumns` share their element numbers.

A wildcard can also target one attribute across every plane:

```
REMOVE (60xx,3000)   # OverlayData only, all planes
REMOVE (60xx,4000)   # OverlayComments only, all planes
```

Note that removing an overlay group does not clear overlays *embedded* in
the high bits of PixelData (the retired mechanism signalled by
`OverlayBitPosition`); that is pixel-side work.

Since `KEEP` outranks `REMOVE` (r-3-11), an explicit `KEEP` protects
individual tags from a blanket wildcard rule:

```
REMOVE (60xx,xxxx)
KEEP (6000,1301)     # but retain this ROI measurement
```

## File Meta Information

The File Meta Information group (group 0002) is de-identified automatically on
every file, regardless of recipe content, and recipes cannot address it. This
follows DICOM PS3.15 E.1.1 and matches CTP, which rebuilds the group from the
de-identified data set.

| Tag                                     | Handling                                     |
|-----------------------------------------|----------------------------------------------|
| `(0002,0002)` MediaStorageSOPClassUID    | Set from the data set's `SOPClassUID`        |
| `(0002,0003)` MediaStorageSOPInstanceUID | Set from the data set's `SOPInstanceUID`     |
| `(0002,0010)` TransferSyntaxUID          | Preserved (governs readability)              |
| `(0002,0012)` ImplementationClassUID     | Stamped with this tool's identity            |
| `(0002,0013)` ImplementationVersionName  | Stamped with this tool's version             |
| `(0002,0016/0017/0018)` AE Titles        | Removed                                      |
| `(0002,0100)`/`(0002,0102)` Private Info | Removed                                      |

Without this, `REPLACE SOPInstanceUID func:hashuid` would hash `(0008,0018)`
while leaving the original UID in `(0002,0003)`, so every output file would
still carry a key linking it back to the source archive.

A data set left with no `SOPInstanceUID` cannot satisfy this and **aborts the
run** rather than being skipped — it indicates a malformed recipe or corrupt
input, so continuing would produce a whole run of suspect output.

## Library Usage

```rust
use dicom_deid_rs::layout::DEID_PATH_LAYOUT;
use dicom_deid_rs::pipeline::{DeidConfig, DeidPipeline};
use std::collections::HashMap;
use std::path::PathBuf;

let config = DeidConfig {
    input_dir: PathBuf::from("./input"),
    output_dir: PathBuf::from("./output"),
    recipe_path: PathBuf::from("recipe.txt"),
    variables: HashMap::from([
        ("PATIENT_ID".into(), "ANON-001".into()),
    ]),
    functions: HashMap::new(), // hashuid is built-in
    salt: Some("my-secret-salt".into()), // or None for unsalted hashuid
    // None mirrors the input tree; a template names outputs from
    // de-identified values. Any {Tag} template is accepted.
    output_layout: Some(DEID_PATH_LAYOUT.into()),
    // PHI: must be outside output_dir.
    mapping_file: Some(PathBuf::from("./keys/mapping.tsv")),
};

let pipeline = DeidPipeline::new(config).unwrap();
let report = pipeline.run().unwrap();
println!("Processed: {}, Blacklisted: {}", report.files_processed, report.files_blacklisted);
```

Custom functions can be supplied via `config.functions` to extend the recipe with application-specific logic.

A PatientID mapper is layered over the same config; the recipe at
`config.recipe_path` still runs in full. The mapper can come from a file or
from pairs already in memory, with the same validation either way:

```rust
use dicom_deid_rs::mapper::PatientIdMapper;
use dicom_deid_rs::pipeline::DeidPipeline;
use std::path::Path;

// From a .csv or .json file...
let pipeline = DeidPipeline::with_mapper_file(config, Path::new("./keys/ids.csv")).unwrap();

// ...or from pairs the caller already holds.
let mapper = PatientIdMapper::from_pairs([("MRN0012345", "ANON-0001")]).unwrap();
let pipeline = DeidPipeline::with_mapper(config, mapper).unwrap();
```

## Building

```
cargo build --release
```

### Feature flags

| Feature    | Default | Description                          |
|------------|---------|--------------------------------------|
| `jpeg2000` | yes     | JPEG 2000 decompression via OpenJPEG |

To build without JPEG 2000 support:

```
cargo build --release --no-default-features
```

## Testing

```
cargo test
```
