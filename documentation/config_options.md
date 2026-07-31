# Configuration Options: PHI-Safe Output Names, Crosswalks, and Date Shifting

Answers to three questions about `dicom-deid-rs`, based on a review of the
source code (`src/main.rs`, `src/pipeline.rs`, `src/metadata.rs`,
`src/functions.rs`), the README, RUNNING.md, and the shipped recipes.

---

## 1. Can it rename output subdirectories so they do not contain PHI?

**No.** There is no option for this, and the current behavior actively works
against it: the output path is computed by stripping the input directory
prefix and re-joining the remainder onto the output directory
(`src/pipeline.rs:159-163`):

```rust
let relative = file_path.strip_prefix(&self.config.input_dir)?;
let output_path = self.config.output_dir.join(relative);
```

The input directory structure — including every folder name and the original
file name — is **copied verbatim** into the output tree. RUNNING.md confirms
this: *"the input directory structure is preserved."*

**Practical consequence:** if your input tree is organized like
`input/SMITH_JOHN_12345678/CT_2024-01-15/img001.dcm`, the de-identified files
land at `output/SMITH_JOHN_12345678/CT_2024-01-15/img001.dcm` — the metadata
inside the files is cleaned, but the PHI in the path is not. There is no CLI
flag, recipe directive, or library option to rename, hash, or flatten the
directory names.

**Workarounds:**
- Rename/restructure the input directories to non-PHI names *before* running
  the tool (safest).
- Post-process the output tree with a separate renaming script (the PHI paths
  will still have transiently existed on disk).
- Use the library API and call `pipeline.process_file()` yourself — but note
  that `process_file` also writes to the structure-preserving path internally,
  so true control over output naming would require a code change.

---

## 2. Can it create a de-identification crosswalk (linking de-identified values back to PHI)?

**No.** The tool never writes a mapping of original → de-identified values.
The only per-file record it produces is `blacklisted_files.txt` in the output
directory (`src/pipeline.rs:282-291`), which lists files that were *excluded*
from output and the filter rule that matched — it contains no value mappings.

Two nuances worth understanding:

- **Hashing is deterministic and one-way.** The built-in `hashuid` function
  (`src/functions.rs`) is a SHA-256-based hash (`2.25.<decimal>` DICOM UID
  form). The default recipe (`keep-only-recipe.txt`) applies it to
  `PatientName`, `PatientID`, and all instance/study/series UIDs. Because it
  is deterministic, the same patient always maps to the same hashed value —
  so groupings are preserved and you *could* build a crosswalk yourself by
  hashing your known PHI list and matching outputs. But the tool cannot
  reverse a hash, and it never emits the pairing for you.
- **Variables give you a manual crosswalk path.** If you use
  `REPLACE PatientID var:PATIENT_ID` with `--var PATIENT_ID ANON-001`, *you*
  choose the replacement value per run, and your own records of which
  `--var` values you used per subject become the crosswalk. This only works
  if you run the tool once per subject (one fixed replacement value per
  invocation) — there is no built-in per-patient ID assignment or lookup
  table.

**If you need a real crosswalk:** generate it externally — e.g., precompute
`original PatientID → study ID` in a spreadsheet/database, run the tool once
per subject with `--var` values from that table, and store the table
securely. Alternatively, the library API accepts custom functions
(`config.functions`), so an embedding application could supply a
`func:` implementation that records every input/output pair it sees — but
that requires writing Rust code around the library; nothing in the shipped
CLI does it.

---

## 3. What are the date and time shift options, and how do I use them?

The single date-shifting mechanism is the **`JITTER`** header action
(`src/metadata.rs:73-107`).

### What JITTER does

- Shifts a date-valued tag by a fixed number of **whole days** (positive =
  forward, negative = backward; negative values are covered by unit tests).
- Works on `DA` (date, `YYYYMMDD`) and `DT` (datetime) values. For `DT`
  values, only the first 8 characters (the date) are shifted; the
  **time-of-day suffix is preserved unchanged**.
- Blank/empty date values are left alone (no-op); if the tag is absent,
  nothing happens.
- **There is no time-shifting option.** Pure `TM` (time-only) tags cannot be
  jittered — a time value will fail the `YYYYMMDD` date parse, which errors
  the file (it is then counted as *skipped* and not written). Only use
  `JITTER` on date/datetime tags.
- The offset is **one constant for the whole run** — every patient and every
  study in the batch is shifted by the same amount. There is no per-patient
  random jitter.

### How to use it

**In the recipe** (`%header` section), one line per tag, with the day count
as either a literal or a variable:

```
%header
JITTER StudyDate 30              # literal: shift +30 days
JITTER StudyDate var:DATEINC     # variable: value supplied at run time
```

**On the command line**, supply the variable:

```sh
dicom-deid-rs ./input ./output recipe.txt --var DATEINC 30
# or shift backwards:
dicom-deid-rs ./input ./output recipe.txt --var DATEINC -45
```

**In the shipped keep-only recipe** (`keep-only-recipe.txt`), dates are
blanked/removed by default, and a ready-made jitter block is provided
commented out (around lines 216–222). To switch to date shifting:

1. Comment out the corresponding `BLANK`/`REMOVE` date lines (see the
   recipe's section 4 notes).
2. Uncomment the JITTER block:
   ```
   JITTER StudyDate var:DATEINC
   JITTER SeriesDate var:DATEINC
   JITTER AcquisitionDate var:DATEINC
   JITTER ContentDate var:DATEINC
   JITTER InstanceCreationDate var:DATEINC
   JITTER AcquisitionDateTime var:DATEINC
   ```
3. Run with `--var DATEINC <days>`.

### Interactions to be aware of

- **Precedence:** when multiple actions target the same tag,
  `KEEP > ADD > REPLACE > JITTER > REMOVE > BLANK`. A `KEEP`, `ADD`, or
  `REPLACE` rule on the same tag silently wins over your `JITTER`; `JITTER`
  in turn wins over `REMOVE`/`BLANK`. Make sure no higher-precedence rule
  targets the date tags you want shifted.
- **Consistency:** because the shift is deterministic and uniform, intervals
  between studies are preserved (useful for longitudinal research), but a
  single known true date can reveal the offset for the entire batch. If you
  need per-patient offsets, run the tool once per patient with a different
  `--var DATEINC` value each time, and record those offsets in your own
  (secured) crosswalk table.
