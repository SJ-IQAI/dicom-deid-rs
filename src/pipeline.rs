use crate::error::DeidError;
use crate::filter_index::FilterIndex;
use crate::functions;
use crate::layout::OutputLayout;
use crate::mapper::PatientIdMapper;
use crate::metadata;
use crate::metadata::DeidFunction;
use crate::pixel;
use crate::recipe::{ActionType, Recipe, TagSpecifier};
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::{Tag, VR};
use dicom_dictionary_std::StandardDataDictionary;
use dicom_object::open_file;
use indicatif::{ProgressBar, ProgressStyle};
use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Mutex;
#[cfg(feature = "parallel")]
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// The name of the blacklist report, written to the current working
/// directory (r-1-11).
pub const BLACKLIST_REPORT_NAME: &str = "blacklisted_files.txt";

/// Configuration for the de-identification pipeline.
pub struct DeidConfig {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub recipe_path: PathBuf,
    pub variables: HashMap<String, String>,
    pub functions: HashMap<String, DeidFunction>,
    /// Optional salt for the built-in `hashuid` function, prepended
    /// to the value before SHA-256 hashing (`SHA-256(salt + value)`),
    /// matching the companion Python implementation. Must be kept
    /// stable across runs to preserve longitudinal consistency of
    /// hashed values. `None` preserves unsalted (plain SHA-256)
    /// behavior.
    pub salt: Option<String>,
    /// Optional output path template built from de-identified tag
    /// values, e.g.
    /// `{PatientID}/{StudyInstanceUID}/{SeriesInstanceUID}_{SeriesNumber}/{SOPInstanceUID}.dcm`
    /// (see [`crate::layout::DEID_PATH_LAYOUT`]).
    ///
    /// `None` mirrors the input directory structure into the output
    /// directory (r-1-4). Since input paths are typically named after
    /// the very identifiers being removed, a layout should be supplied
    /// whenever the output tree must itself be free of PHI.
    pub output_layout: Option<String>,
    /// Optional path for the original-to-de-identified path mapping
    /// (r-1-10). The mapping is PHI by construction — it names the
    /// input files — so it must live outside `output_dir`; the pipeline
    /// refuses to start otherwise.
    pub mapping_file: Option<PathBuf>,
}

/// Summary report after de-identification completes.
pub struct DeidReport {
    pub files_processed: usize,
    pub files_skipped: usize,
    pub files_blacklisted: usize,
    /// Where the blacklist report was written, if any files were
    /// blacklisted.
    pub blacklist_report_path: Option<PathBuf>,
    /// Where the path mapping was written, if one was requested and any
    /// files were processed.
    pub mapping_file_path: Option<PathBuf>,
}

impl DeidReport {
    fn new() -> Self {
        DeidReport {
            files_processed: 0,
            files_skipped: 0,
            files_blacklisted: 0,
            blacklist_report_path: None,
            mapping_file_path: None,
        }
    }
}

/// The main de-identification pipeline.
pub struct DeidPipeline {
    pub config: DeidConfig,
    pub recipe: Recipe,
    filter_index: FilterIndex,
    layout: Option<OutputLayout>,
    /// When set, the pipeline runs in mapper mode (r-7-2): the only
    /// de-identification applied to the data set is the PatientID
    /// substitution this table defines.
    mapper: Option<PatientIdMapper>,
    /// Output paths already claimed during this run, used to detect
    /// collisions (r-1-9). Only populated when a layout is in use;
    /// without one, input paths are unique so outputs are too.
    claimed_paths: Mutex<HashSet<PathBuf>>,
}

pub enum FileOutcome {
    /// The file was written to the given output path.
    Processed(PathBuf),
    Blacklisted(String),
}

impl DeidPipeline {
    /// Create a new pipeline, parsing the recipe from the configured path.
    ///
    /// Built-in functions (e.g. `hashuid`) are registered automatically.
    /// User-supplied functions in `config.functions` take precedence over
    /// built-in functions with the same name.
    pub fn new(config: DeidConfig) -> Result<Self, DeidError> {
        let recipe_text = fs::read_to_string(&config.recipe_path)?;
        let recipe = Recipe::parse(&recipe_text)?;
        Self::assemble(recipe, config, None)
    }

    /// Create a new pipeline from recipe text directly (avoids temp files).
    pub fn from_recipe_text(recipe_text: &str, config: DeidConfig) -> Result<Self, DeidError> {
        let recipe = Recipe::parse(recipe_text)?;
        Self::assemble(recipe, config, None)
    }

    /// Create a pipeline in mapper mode from a mapper file (r-7-1).
    ///
    /// The file is read and validated here, so a malformed or empty
    /// mapper is reported before any DICOM file is processed.
    /// `config.recipe_path` is not read: mapper mode replaces the
    /// recipe rather than supplementing it (r-7-2).
    pub fn from_mapper_file(mapper_path: &Path, config: DeidConfig) -> Result<Self, DeidError> {
        Self::from_mapper(PatientIdMapper::load(mapper_path)?, config)
    }

    /// Create a pipeline in mapper mode from a mapper built in memory
    /// (r-6-1, r-7-2).
    ///
    /// The only de-identification applied to the data set is the
    /// PatientID substitution: no recipe actions, no filter evaluation,
    /// no pixel masking, and no private tag removal. File Meta
    /// Information de-identification (r-3-14) still applies, since it is
    /// unconditional for every written file.
    pub fn from_mapper(mapper: PatientIdMapper, config: DeidConfig) -> Result<Self, DeidError> {
        let empty = Recipe {
            format: "dicom".to_string(),
            header: Vec::new(),
            filters: Vec::new(),
        };
        Self::assemble(empty, config, Some(mapper))
    }

    fn assemble(
        recipe: Recipe,
        mut config: DeidConfig,
        mapper: Option<PatientIdMapper>,
    ) -> Result<Self, DeidError> {
        let mut merged = functions::default_functions(config.salt.as_deref());
        for (name, func) in config.functions.drain() {
            merged.insert(name, func);
        }
        config.functions = merged;

        let layout = match &config.output_layout {
            Some(template) => Some(OutputLayout::parse(template)?),
            None => None,
        };
        if let Some(layout) = &layout {
            warn_on_unprotected_layout_tags(layout, &recipe, mapper.is_some());
        }
        if let Some(mapping_file) = &config.mapping_file {
            validate_mapping_file(mapping_file, &config.output_dir)?;
        }

        let filter_index = FilterIndex::new(&recipe);
        Ok(DeidPipeline {
            config,
            recipe,
            filter_index,
            layout,
            mapper,
            claimed_paths: Mutex::new(HashSet::new()),
        })
    }

    /// Recursively search a directory for DICOM files.
    pub fn find_dicom_files(dir: &Path) -> Result<Vec<PathBuf>, DeidError> {
        let mut results = Vec::new();
        find_dicom_files_recursive(dir, &mut results)?;
        Ok(results)
    }

    /// Run the de-identification pipeline.
    pub fn run(&self) -> Result<DeidReport, DeidError> {
        let files = Self::find_dicom_files(&self.config.input_dir)?;
        let pb = ProgressBar::new(files.len() as u64);
        pb.set_style(
            ProgressStyle::with_template("[{elapsed_precise}] [{bar:40}] {pos}/{len} ({eta})")
                .expect("valid progress bar template")
                .progress_chars("=> "),
        );

        self.reset_claimed_paths();
        let mut report = DeidReport::new();
        let mut blacklisted_files: Vec<(PathBuf, String)> = Vec::new();
        let mut mappings: Vec<(PathBuf, PathBuf)> = Vec::new();

        for file_path in &files {
            match self.process_file(file_path) {
                Ok(FileOutcome::Processed(output_path)) => {
                    report.files_processed += 1;
                    if self.config.mapping_file.is_some() {
                        mappings.push((file_path.clone(), output_path));
                    }
                }
                Ok(FileOutcome::Blacklisted(reason)) => {
                    let relative = file_path
                        .strip_prefix(&self.config.input_dir)
                        .unwrap_or(file_path);
                    blacklisted_files.push((relative.to_path_buf(), reason));
                    report.files_blacklisted += 1;
                }
                Err(e) if e.is_fatal() => {
                    pb.abandon_with_message("De-identification aborted");
                    return Err(e);
                }
                Err(e) => {
                    pb.println(format!("Warning: skipping {}: {}", file_path.display(), e));
                    report.files_skipped += 1;
                }
            }
            pb.inc(1);
        }

        pb.finish_with_message("De-identification complete");

        self.write_reports(&mut report, &blacklisted_files, &mappings)?;
        Ok(report)
    }

    pub fn process_file(&self, file_path: &Path) -> Result<FileOutcome, DeidError> {
        let mut obj = open_file(file_path).map_err(|e| {
            DeidError::Dicom(format!("failed to open {}: {}", file_path.display(), e))
        })?;

        match &self.mapper {
            // Mapper mode (r-7-2): substituting PatientID is the whole
            // of the de-identification. Filters, pixel masking, header
            // actions, and private tag removal are all deliberately
            // skipped — the caller asked for this file to be left alone
            // apart from its identifier. A value with no entry in the
            // mapper returns a non-fatal error here, so the file is
            // counted as skipped and never written (r-7-6).
            Some(mapper) => mapper.apply(&mut obj)?,
            None => {
                // Check blacklist, unless an allowlist rule exempts this file (r-5-2).
                //
                // The exemption stops here: it suppresses *rejection* only. Masking and
                // header de-identification below still run, because a device whitelist
                // exists precisely to admit devices that carry burned-in PHI and leave
                // the graylist to mask it. Short-circuiting past the next two blocks
                // would emit that PHI.
                if !self.filter_index.is_allowlisted(&obj)
                    && let Some(reason) = self.filter_index.blacklist_reason(&obj)
                {
                    return Ok(FileOutcome::Blacklisted(reason.to_string()));
                }

                // Pixel de-identification
                let regions = self.filter_index.get_graylist_regions(&obj);
                if !regions.is_empty() {
                    pixel::decompress_pixel_data(&mut obj)?;
                    pixel::apply_pixel_mask(&mut obj, &regions)?;
                }

                // Metadata de-identification
                metadata::apply_header_actions(
                    &self.recipe.header,
                    &self.config.variables,
                    &self.config.functions,
                    &mut obj,
                )?;
                metadata::remove_private_tags(&mut obj);
            }
        }

        // Bring the file meta group in line with the de-identified data
        // set. Must come last: it reads the final data set values, and
        // it runs after pixel decompression has settled the transfer
        // syntax.
        metadata::sync_file_meta(&mut obj)?;

        // Compute the output path. This must come after every action
        // above: with a layout, the path components are read back out of
        // the de-identified data set, so the values used are the ones
        // that were just written into the file.
        let relative = match &self.layout {
            Some(layout) => layout.render(&obj)?,
            None => file_path
                .strip_prefix(&self.config.input_dir)
                .map_err(|e| DeidError::Io(std::io::Error::other(e)))?
                .to_path_buf(),
        };
        let output_path = self.config.output_dir.join(&relative);

        // A layout can map two inputs onto one path — most easily when a
        // recipe blanks or removes one of the identifiers the layout
        // reads, which would collapse a whole study onto a single file.
        // Claim the path first so the earlier file is never silently
        // overwritten (r-1-9).
        if self.layout.is_some() {
            let mut claimed = self.claimed_paths.lock().unwrap_or_else(|e| e.into_inner());
            if !claimed.insert(output_path.clone()) {
                return Err(DeidError::PathCollision(format!(
                    "{} already written by another input file",
                    relative.display()
                )));
            }
        }

        if let Some(parent) = output_path.parent() {
            fs::create_dir_all(parent)?;
        }

        obj.write_to_file(&output_path).map_err(|e| {
            DeidError::Dicom(format!("failed to write {}: {}", output_path.display(), e))
        })?;

        Ok(FileOutcome::Processed(output_path))
    }

    /// Run the pipeline with a progress callback instead of a progress bar.
    pub fn run_with_progress(
        &self,
        on_progress: impl Fn(usize, usize, usize),
    ) -> Result<DeidReport, DeidError> {
        let files = Self::find_dicom_files(&self.config.input_dir)?;
        self.reset_claimed_paths();
        let mut report = DeidReport::new();
        let mut blacklisted_files: Vec<(PathBuf, String)> = Vec::new();
        let mut mappings: Vec<(PathBuf, PathBuf)> = Vec::new();

        for file_path in &files {
            match self.process_file(file_path) {
                Ok(FileOutcome::Processed(output_path)) => {
                    report.files_processed += 1;
                    if self.config.mapping_file.is_some() {
                        mappings.push((file_path.clone(), output_path));
                    }
                }
                Ok(FileOutcome::Blacklisted(reason)) => {
                    let relative = file_path
                        .strip_prefix(&self.config.input_dir)
                        .unwrap_or(file_path);
                    blacklisted_files.push((relative.to_path_buf(), reason));
                    report.files_blacklisted += 1;
                }
                Err(e) if e.is_fatal() => return Err(e),
                Err(e) => {
                    eprintln!("Warning: skipping {}: {}", file_path.display(), e);
                    report.files_skipped += 1;
                }
            }
            on_progress(
                report.files_processed,
                report.files_blacklisted,
                report.files_skipped,
            );
        }

        self.write_reports(&mut report, &blacklisted_files, &mappings)?;
        Ok(report)
    }

    /// Run the pipeline using parallel file processing via rayon.
    #[cfg(feature = "parallel")]
    pub fn run_parallel(
        &self,
        num_threads: usize,
        on_progress: impl Fn(usize, usize, usize) + Send + Sync,
    ) -> Result<DeidReport, DeidError> {
        use rayon::prelude::*;

        let files = Self::find_dicom_files(&self.config.input_dir)?;
        self.reset_claimed_paths();

        let pool = rayon::ThreadPoolBuilder::new()
            .num_threads(num_threads)
            .build()
            .map_err(|e| DeidError::Io(std::io::Error::other(e)))?;

        let processed = AtomicUsize::new(0);
        let blacklisted_count = AtomicUsize::new(0);
        let skipped = AtomicUsize::new(0);

        let blacklisted_files: Mutex<Vec<(PathBuf, String)>> = Mutex::new(Vec::new());
        let mappings: Mutex<Vec<(PathBuf, PathBuf)>> = Mutex::new(Vec::new());

        // `for_each` cannot early-return, so a fatal error sets an abort
        // flag that the remaining items check on entry. Files already in
        // flight still finish; that is acceptable on an abort path.
        let abort = AtomicBool::new(false);
        let fatal: std::sync::Mutex<Option<DeidError>> = std::sync::Mutex::new(None);

        pool.install(|| {
            files.par_iter().for_each(|file_path| {
                if abort.load(Ordering::Relaxed) {
                    return;
                }
                match self.process_file(file_path) {
                    Ok(FileOutcome::Processed(output_path)) => {
                        processed.fetch_add(1, Ordering::Relaxed);
                        if self.config.mapping_file.is_some() {
                            mappings
                                .lock()
                                .unwrap()
                                .push((file_path.clone(), output_path));
                        }
                    }
                    Ok(FileOutcome::Blacklisted(reason)) => {
                        let relative = file_path
                            .strip_prefix(&self.config.input_dir)
                            .unwrap_or(file_path);
                        blacklisted_files
                            .lock()
                            .unwrap()
                            .push((relative.to_path_buf(), reason));
                        blacklisted_count.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) if e.is_fatal() => {
                        abort.store(true, Ordering::Relaxed);
                        let mut slot = fatal.lock().unwrap();
                        if slot.is_none() {
                            *slot = Some(e);
                        }
                        return;
                    }
                    Err(e) => {
                        eprintln!("Warning: skipping {}: {}", file_path.display(), e);
                        skipped.fetch_add(1, Ordering::Relaxed);
                    }
                }
                on_progress(
                    processed.load(Ordering::Relaxed),
                    blacklisted_count.load(Ordering::Relaxed),
                    skipped.load(Ordering::Relaxed),
                );
            });
        });

        // Abort before writing any report: on a fatal error the run's
        // output is suspect as a whole.
        if let Some(e) = fatal.into_inner().unwrap() {
            return Err(e);
        }

        let mut report = DeidReport {
            files_processed: processed.into_inner(),
            files_skipped: skipped.into_inner(),
            files_blacklisted: blacklisted_count.into_inner(),
            blacklist_report_path: None,
            mapping_file_path: None,
        };
        let blacklisted_files = blacklisted_files.into_inner().unwrap();
        let mut mappings = mappings.into_inner().unwrap();
        // Rayon completes files out of order; sort so the mapping file
        // is byte-identical to the sequential runner's.
        mappings.sort();

        self.write_reports(&mut report, &blacklisted_files, &mappings)?;
        Ok(report)
    }

    /// Forget the output paths claimed by a previous run so the same
    /// pipeline can be run more than once.
    fn reset_claimed_paths(&self) {
        self.claimed_paths
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    fn write_reports(
        &self,
        report: &mut DeidReport,
        blacklisted: &[(PathBuf, String)],
        mappings: &[(PathBuf, PathBuf)],
    ) -> Result<(), DeidError> {
        if !blacklisted.is_empty() {
            report.blacklist_report_path = Some(self.write_blacklist_report(blacklisted)?);
        }
        if let Some(mapping_file) = &self.config.mapping_file
            && !mappings.is_empty()
        {
            write_mapping_file(mapping_file, mappings)?;
            report.mapping_file_path = Some(mapping_file.clone());
        }
        Ok(())
    }

    /// Write the blacklist report to the current working directory.
    ///
    /// It names the *input* files, which are PHI, and blacklisted files
    /// are never de-identified so there is no safe name to use instead.
    /// Keeping it out of the output directory is what lets that
    /// directory stay free of PHI (r-1-11).
    fn write_blacklist_report(
        &self,
        blacklisted: &[(PathBuf, String)],
    ) -> Result<PathBuf, DeidError> {
        let report_path = std::env::current_dir()?.join(BLACKLIST_REPORT_NAME);
        let mut lines = Vec::with_capacity(blacklisted.len());
        for (path, reason) in blacklisted {
            lines.push(format!("{}\t{}", path.display(), reason));
        }
        fs::write(&report_path, lines.join("\n") + "\n")?;
        Ok(report_path)
    }
}

/// Write the original-to-de-identified path mapping (r-1-10).
///
/// The left column is PHI by construction, so the caller is responsible
/// for placing this file somewhere appropriately protected; the pipeline
/// only enforces that it is not inside the output directory.
fn write_mapping_file(
    mapping_file: &Path,
    mappings: &[(PathBuf, PathBuf)],
) -> Result<(), DeidError> {
    if let Some(parent) = mapping_file.parent()
        && !parent.as_os_str().is_empty()
    {
        fs::create_dir_all(parent)?;
    }
    let mut out = String::from("original_path\tdeidentified_path\n");
    for (input, output) in mappings {
        out.push_str(&format!("{}\t{}\n", input.display(), output.display()));
    }
    fs::write(mapping_file, out)?;
    Ok(())
}

/// Reject a mapping file inside the output directory: it names the input
/// files, so writing it there would put PHI back into the tree the
/// layout exists to keep clean (r-1-10).
fn validate_mapping_file(mapping_file: &Path, output_dir: &Path) -> Result<(), DeidError> {
    let mapping = normalize_lexically(mapping_file);
    let output = normalize_lexically(output_dir);
    if mapping.starts_with(&output) {
        return Err(DeidError::Layout(format!(
            "mapping file {} is inside the output directory {}; it records original \
             file paths and must be stored outside the de-identified output",
            mapping_file.display(),
            output_dir.display()
        )));
    }
    Ok(())
}

/// Make a path absolute and fold away `.` and `..` components.
///
/// This is purely lexical — it does not touch the filesystem, since
/// neither path is required to exist yet — so it is a guard against
/// mistakes, not against a deliberately adversarial symlink.
fn normalize_lexically(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut out = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Warn when a layout reads a tag the recipe never de-identifies.
///
/// The layout only keeps PHI out of the output paths if the values it
/// reads have actually been changed. A tag left untouched (or explicitly
/// KEEP'd) puts the original identifier straight into the path, which is
/// exactly the failure the layout exists to prevent. This warns rather
/// than refuses, since a site may legitimately feed in already-
/// pseudonymous identifiers.
fn warn_on_unprotected_layout_tags(layout: &OutputLayout, recipe: &Recipe, mapper_mode: bool) {
    let dict = StandardDataDictionary;
    for tag in unprotected_layout_tags(layout, recipe, mapper_mode) {
        let name = dict
            .by_tag(tag)
            .map(|e| e.alias().to_string())
            .unwrap_or_else(|| format!("({:04X},{:04X})", tag.0, tag.1));
        eprintln!(
            "Warning: output layout uses {} but the recipe has no action that \
             de-identifies it; output paths may contain PHI",
            name
        );
    }
}

/// The layout tags no recipe action de-identifies. See
/// [`warn_on_unprotected_layout_tags`] for why this is a warning.
fn unprotected_layout_tags(layout: &OutputLayout, recipe: &Recipe, mapper_mode: bool) -> Vec<Tag> {
    let deidentifying: Vec<&TagSpecifier> = recipe
        .header
        .iter()
        .filter(|a| {
            matches!(
                a.action_type,
                // ADD only fires when the tag is absent and KEEP is a
                // no-op, so neither de-identifies an existing value.
                ActionType::Replace | ActionType::Jitter | ActionType::Blank | ActionType::Remove
            )
        })
        .map(|a| &a.tag)
        .collect();

    let dict = StandardDataDictionary;
    layout
        .tags()
        .into_iter()
        .filter(|tag| {
            // In mapper mode the recipe is empty, but PatientID is
            // still replaced on every written file (r-7-2), so it does
            // not put an original identifier into the path.
            !(mapper_mode && *tag == dicom_dictionary_std::tags::PATIENT_ID)
        })
        .filter(|tag| {
            // A specifier that needs the data set to resolve might well
            // cover this tag; stay quiet rather than cry wolf.
            if deidentifying.iter().any(|s| s.matches(*tag).is_none()) {
                return false;
            }
            !deidentifying.iter().any(|s| s.matches(*tag) == Some(true))
        })
        .filter(|tag| {
            // A numeric VR cannot carry a name, an identifier, or a
            // date, so leaving it alone is not a PHI leak. SeriesNumber
            // (VR IS) is in the canonical layout and is never
            // de-identified; warning about it every run would train
            // operators to ignore this warning.
            let vr = dict
                .by_tag(*tag)
                .map(|e| e.vr().relaxed())
                .unwrap_or(VR::LO);
            can_carry_phi(vr)
        })
        .collect()
}

/// Whether a VR can hold identifying text, a UID, or a date.
fn can_carry_phi(vr: VR) -> bool {
    matches!(
        vr,
        VR::AE
            | VR::AS
            | VR::CS
            | VR::DA
            | VR::DT
            | VR::LO
            | VR::LT
            | VR::PN
            | VR::SH
            | VR::ST
            | VR::TM
            | VR::UC
            | VR::UI
            | VR::UR
            | VR::UT
    )
}

fn find_dicom_files_recursive(dir: &Path, results: &mut Vec<PathBuf>) -> Result<(), DeidError> {
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            find_dicom_files_recursive(&path, results)?;
        } else if path
            .extension()
            .is_some_and(|ext| ext.eq_ignore_ascii_case("dcm"))
        {
            results.push(path);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // -- r-1-1 ---------------------------------------------------------------

    /// Requirement r-1-1
    #[test]
    fn r1_1_config_accepts_required_paths() {
        let config = DeidConfig {
            input_dir: PathBuf::from("/tmp/input"),
            output_dir: PathBuf::from("/tmp/output"),
            recipe_path: PathBuf::from("/tmp/recipe.txt"),
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        };
        assert_eq!(config.input_dir, PathBuf::from("/tmp/input"));
        assert_eq!(config.output_dir, PathBuf::from("/tmp/output"));
        assert_eq!(config.recipe_path, PathBuf::from("/tmp/recipe.txt"));
    }

    // -- r-1-2 ---------------------------------------------------------------

    /// Requirement r-1-2
    #[test]
    fn r1_2_recursive_search_finds_dcm_files() {
        let tmp = TempDir::new().expect("should create temp dir");
        let root = tmp.path();

        // Create nested directory structure with .dcm files
        let sub1 = root.join("sub1");
        let sub2 = root.join("sub1").join("sub2");
        fs::create_dir_all(&sub2).expect("should create dirs");

        fs::write(root.join("file1.dcm"), b"DICM").expect("write");
        fs::write(sub1.join("file2.dcm"), b"DICM").expect("write");
        fs::write(sub2.join("file3.dcm"), b"DICM").expect("write");

        // Also create a non-DICOM file to ensure it's excluded
        fs::write(root.join("notes.txt"), b"not a dicom file").expect("write");

        let files = DeidPipeline::find_dicom_files(root).expect("should find files");
        assert_eq!(files.len(), 3, "should find all 3 .dcm files recursively");
    }

    /// Requirement r-1-2
    #[test]
    fn r1_2_empty_directory_returns_empty() {
        let tmp = TempDir::new().expect("should create temp dir");
        let files = DeidPipeline::find_dicom_files(tmp.path()).expect("should handle empty dir");
        assert!(files.is_empty());
    }

    /// Requirement r-1-2
    #[test]
    fn r1_2_find_skips_non_dcm_files() {
        let tmp = TempDir::new().expect("should create temp dir");
        let root = tmp.path();

        fs::write(root.join("image.dcm"), b"DICM").expect("write");
        fs::write(root.join("readme.txt"), b"text").expect("write");
        fs::write(root.join("data.json"), b"{}").expect("write");
        fs::write(root.join("report.pdf"), b"PDF").expect("write");

        let files = DeidPipeline::find_dicom_files(root).expect("should find files");
        assert_eq!(files.len(), 1, "should only find .dcm files");
        assert!(
            files[0]
                .file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .ends_with(".dcm")
        );
    }

    // -- r-1-3 ---------------------------------------------------------------

    /// Requirement r-1-3: full pipeline run with a valid DICOM file
    #[test]
    fn r1_3_run_processes_dicom_file() {
        use crate::test_helpers::*;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        fs::create_dir_all(&input_dir).expect("create input dir");

        // Create a minimal valid DICOM file
        let mut file_obj = create_test_file_obj();
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::PATIENT_NAME,
            dicom_core::VR::PN,
            "John^Doe",
        );
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::MODALITY,
            dicom_core::VR::CS,
            "CT",
        );
        // Type 1: required for the file meta sync (r-3-14)
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            dicom_core::VR::UI,
            "1.2.3.4.5.6.7.8.9",
        );
        let dcm_path = input_dir.join("test.dcm");
        file_obj
            .write_to_file(&dcm_path)
            .expect("write test DICOM file");

        // Create a minimal recipe file
        let recipe_path = tmp.path().join("recipe.txt");
        fs::write(
            &recipe_path,
            "FORMAT dicom\n%header\nREPLACE PatientName ANON\n",
        )
        .expect("write recipe");

        let config = DeidConfig {
            input_dir: input_dir.clone(),
            output_dir: output_dir.clone(),
            recipe_path,
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        };

        let pipeline = DeidPipeline::new(config).expect("should create pipeline");
        let report = pipeline.run().expect("should run pipeline");

        assert_eq!(report.files_processed, 1);
        assert_eq!(report.files_skipped, 0);
        assert_eq!(report.files_blacklisted, 0);

        // Verify output file exists
        let output_file = output_dir.join("test.dcm");
        assert!(output_file.exists(), "output file should exist");

        // Verify the patient name was replaced
        let result_obj = open_file(&output_file).expect("should open output");
        let name = result_obj
            .element_by_name("PatientName")
            .expect("should have PatientName");
        let val = name.value().to_str().expect("should read value");
        assert_eq!(val.as_ref(), "ANON");
    }

    // -- r-6-1 ---------------------------------------------------------------

    /// Requirement r-6-1
    #[test]
    fn r6_1_library_api_is_accessible() {
        use crate::recipe::{
            ActionType, ActionValue, Condition, FilterLabel, FilterSection, FilterType,
            HeaderAction, LogicalOp, Predicate, Recipe, TagSpecifier,
        };

        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![HeaderAction {
                action_type: ActionType::Add,
                tag: TagSpecifier::Keyword("PatientIdentityRemoved".into()),
                value: Some(ActionValue::Literal("YES".into())),
            }],
            filters: vec![FilterSection {
                filter_type: FilterType::Blacklist,
                labels: vec![FilterLabel {
                    name: "Test".into(),
                    conditions: vec![Condition {
                        operator: LogicalOp::First,
                        predicate: Predicate::Missing {
                            field: "Modality".into(),
                        },
                    }],
                    coordinates: vec![],
                }],
            }],
        };

        assert_eq!(recipe.format, "dicom");
        assert_eq!(recipe.header.len(), 1);
        assert_eq!(recipe.filters.len(), 1);
    }

    // -- r-1-3 (parallel) ----------------------------------------------------

    /// Requirement r-1-3: run_parallel produces same results as sequential run
    #[cfg(feature = "parallel")]
    #[test]
    fn r1_3_run_parallel_produces_same_results() {
        use crate::test_helpers::*;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        fs::create_dir_all(&input_dir).expect("create input dir");

        for i in 0..5 {
            let mut file_obj = create_test_file_obj();
            put_str(
                &mut file_obj,
                dicom_dictionary_std::tags::PATIENT_NAME,
                dicom_core::VR::PN,
                &format!("Patient^{}", i),
            );
            put_str(
                &mut file_obj,
                dicom_dictionary_std::tags::MODALITY,
                dicom_core::VR::CS,
                "CT",
            );
            // Type 1: required for the file meta sync (r-3-14)
            put_str(
                &mut file_obj,
                dicom_dictionary_std::tags::SOP_INSTANCE_UID,
                dicom_core::VR::UI,
                &format!("1.2.3.4.5.6.7.8.{}", i),
            );
            file_obj
                .write_to_file(input_dir.join(format!("test_{}.dcm", i)))
                .expect("write test DICOM file");
        }

        let recipe_path = tmp.path().join("recipe.txt");
        fs::write(
            &recipe_path,
            "FORMAT dicom\n%header\nREPLACE PatientName ANON\n",
        )
        .expect("write recipe");

        let config = DeidConfig {
            input_dir: input_dir.clone(),
            output_dir: output_dir.clone(),
            recipe_path,
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        };

        let pipeline = DeidPipeline::new(config).expect("should create pipeline");
        let report = pipeline
            .run_parallel(2, |_, _, _| {})
            .expect("should run parallel pipeline");

        assert_eq!(report.files_processed, 5);
        assert_eq!(report.files_skipped, 0);
        assert_eq!(report.files_blacklisted, 0);

        for i in 0..5 {
            let output_file = output_dir.join(format!("test_{}.dcm", i));
            assert!(output_file.exists(), "output file {} should exist", i);
            let result_obj = open_file(&output_file).expect("should open output");
            let name = result_obj
                .element_by_name("PatientName")
                .expect("should have PatientName");
            let val = name.value().to_str().expect("should read value");
            assert_eq!(val.as_ref(), "ANON");
        }
    }

    // -- r-3-14 ---------------------------------------------------------------

    /// Requirement r-3-14: a full run must leave (0002,0003) equal to the
    /// hashed (0008,0018), with the original UID gone from the meta group.
    #[test]
    fn r3_14_run_syncs_file_meta_with_hashed_sop_instance_uid() {
        use crate::test_helpers::*;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        fs::create_dir_all(&input_dir).expect("create input dir");

        // create_test_file_obj seeds meta (0002,0003) with this value.
        const ORIGINAL_UID: &str = "1.2.3.4.5.6.7.8.9";

        let mut file_obj = create_test_file_obj();
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            dicom_core::VR::UI,
            ORIGINAL_UID,
        );
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::MODALITY,
            dicom_core::VR::CS,
            "CT",
        );
        file_obj
            .write_to_file(input_dir.join("test.dcm"))
            .expect("write test DICOM file");

        let recipe_text = "FORMAT dicom\n%header\nREPLACE SOPInstanceUID func:hashuid\n";
        let config = DeidConfig {
            input_dir,
            output_dir: output_dir.clone(),
            recipe_path: PathBuf::new(),
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        };

        let pipeline =
            DeidPipeline::from_recipe_text(recipe_text, config).expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("should run pipeline");
        assert_eq!(report.files_processed, 1);

        let result = open_file(output_dir.join("test.dcm")).expect("should open output");
        let data_set_uid = result
            .element(dicom_dictionary_std::tags::SOP_INSTANCE_UID)
            .expect("should have SOPInstanceUID")
            .value()
            .to_str()
            .expect("should read value")
            .to_string();

        assert!(
            data_set_uid.starts_with("2.25."),
            "data set UID should be hashed, got {}",
            data_set_uid
        );
        assert_eq!(
            result.meta().media_storage_sop_instance_uid(),
            data_set_uid,
            "(0002,0003) must equal (0008,0018)"
        );
        assert_ne!(
            result.meta().media_storage_sop_instance_uid(),
            ORIGINAL_UID,
            "the original SOP Instance UID must not survive in the meta group"
        );
        assert_eq!(
            result.meta().transfer_syntax(),
            "1.2.840.10008.1.2.1",
            "transfer syntax must be preserved"
        );
    }

    /// Requirement r-3-14: a data set left without a SOP Instance UID
    /// aborts the run rather than being counted as skipped.
    #[test]
    fn r3_14_missing_sop_instance_uid_aborts_run() {
        use crate::test_helpers::*;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        fs::create_dir_all(&input_dir).expect("create input dir");

        let mut file_obj = create_test_file_obj();
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            dicom_core::VR::UI,
            "1.2.3.4.5.6.7.8.9",
        );
        file_obj
            .write_to_file(input_dir.join("test.dcm"))
            .expect("write test DICOM file");

        let recipe_text = "FORMAT dicom\n%header\nREMOVE SOPInstanceUID\n";
        let config = DeidConfig {
            input_dir,
            output_dir: output_dir.clone(),
            recipe_path: PathBuf::new(),
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        };

        let pipeline =
            DeidPipeline::from_recipe_text(recipe_text, config).expect("should create pipeline");
        let err = match pipeline.run_with_progress(|_, _, _| {}) {
            Err(e) => e,
            Ok(_) => panic!("should abort the run"),
        };

        assert!(err.is_fatal(), "should be fatal, not a per-file skip");
        assert!(
            !output_dir.join("test.dcm").exists(),
            "no output file should be written"
        );
    }

    /// Requirement r-3-14: the fatal abort also applies to the parallel
    /// runner, which cannot early-return out of `for_each`.
    #[cfg(feature = "parallel")]
    #[test]
    fn r3_14_missing_sop_instance_uid_aborts_parallel_run() {
        use crate::test_helpers::*;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        fs::create_dir_all(&input_dir).expect("create input dir");

        for i in 0..10 {
            let mut file_obj = create_test_file_obj();
            put_str(
                &mut file_obj,
                dicom_dictionary_std::tags::SOP_INSTANCE_UID,
                dicom_core::VR::UI,
                &format!("1.2.3.4.5.6.7.8.{}", i),
            );
            file_obj
                .write_to_file(input_dir.join(format!("test_{}.dcm", i)))
                .expect("write test DICOM file");
        }

        let recipe_text = "FORMAT dicom\n%header\nREMOVE SOPInstanceUID\n";
        let config = DeidConfig {
            input_dir,
            output_dir: output_dir.clone(),
            recipe_path: PathBuf::new(),
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        };

        let pipeline =
            DeidPipeline::from_recipe_text(recipe_text, config).expect("should create pipeline");
        let err = match pipeline.run_parallel(4, |_, _, _| {}) {
            Err(e) => e,
            Ok(_) => panic!("should abort the run"),
        };

        assert!(err.is_fatal(), "should be fatal, not a per-file skip");
    }

    /// from_recipe_text avoids needing a recipe file on disk
    #[test]
    fn from_recipe_text_works() {
        use crate::test_helpers::*;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        fs::create_dir_all(&input_dir).expect("create input dir");

        let mut file_obj = create_test_file_obj();
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::PATIENT_NAME,
            dicom_core::VR::PN,
            "John^Doe",
        );
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::MODALITY,
            dicom_core::VR::CS,
            "CT",
        );
        // Type 1: required for the file meta sync (r-3-14)
        put_str(
            &mut file_obj,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            dicom_core::VR::UI,
            "1.2.3.4.5.6.7.8.9",
        );
        file_obj
            .write_to_file(input_dir.join("test.dcm"))
            .expect("write test DICOM file");

        let recipe_text = "FORMAT dicom\n%header\nREPLACE PatientName ANON\n";
        let config = DeidConfig {
            input_dir,
            output_dir: output_dir.clone(),
            recipe_path: PathBuf::new(),
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        };

        let pipeline =
            DeidPipeline::from_recipe_text(recipe_text, config).expect("should create pipeline");
        let report = pipeline.run_with_progress(|_, _, _| {});
        assert!(report.is_ok());
        let report = report.unwrap();
        assert_eq!(report.files_processed, 1);

        let result_obj = open_file(output_dir.join("test.dcm")).expect("should open output");
        let val = result_obj
            .element_by_name("PatientName")
            .expect("should have PatientName")
            .value()
            .to_str()
            .expect("should read value");
        assert_eq!(val.as_ref(), "ANON");
    }

    // -- r-1-6 .. r-1-10 -----------------------------------------------------

    use crate::layout::DEID_PATH_LAYOUT;

    /// The recipe used by the output-layout tests: every identifier the
    /// canonical layout reads is hashed, so the rendered paths carry no
    /// PHI.
    const LAYOUT_RECIPE: &str = "\
FORMAT dicom
%header
REPLACE PatientID func:hashuid_ascii
REPLACE StudyInstanceUID func:hashuid
REPLACE SeriesInstanceUID func:hashuid
REPLACE SOPInstanceUID func:hashuid
";

    /// Write an input file under a PHI-named tree, exactly as the
    /// scanners lay it out: `<PatientID>/<StudyUID>/<SeriesUID>_<n>/<SOPUID>.dcm`.
    fn write_phi_named_input(
        input_dir: &Path,
        patient: &str,
        study: &str,
        series: &str,
        series_number: &str,
        sop: &str,
    ) -> PathBuf {
        use crate::test_helpers::*;

        let mut obj = create_test_file_obj();
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::PATIENT_ID,
            VR::LO,
            patient,
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::STUDY_INSTANCE_UID,
            VR::UI,
            study,
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::SERIES_INSTANCE_UID,
            VR::UI,
            series,
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::SERIES_NUMBER,
            VR::IS,
            series_number,
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            VR::UI,
            sop,
        );

        let dir = input_dir
            .join(patient)
            .join(study)
            .join(format!("{}_{}", series, series_number));
        fs::create_dir_all(&dir).expect("create input dirs");
        let path = dir.join(format!("{}.dcm", sop));
        obj.write_to_file(&path).expect("write input file");
        path
    }

    fn layout_config(input_dir: PathBuf, output_dir: PathBuf) -> DeidConfig {
        DeidConfig {
            input_dir,
            output_dir,
            recipe_path: PathBuf::new(),
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: Some(DEID_PATH_LAYOUT.to_string()),
            mapping_file: None,
        }
    }

    /// Collect every `.dcm` path under a directory, relative to it.
    fn output_tree(dir: &Path) -> Vec<PathBuf> {
        let mut files = DeidPipeline::find_dicom_files(dir).unwrap_or_default();
        files.sort();
        files
            .iter()
            .map(|p| p.strip_prefix(dir).expect("under dir").to_path_buf())
            .collect()
    }

    /// Requirement r-1-6: output paths are named from the de-identified
    /// values, and none of the original identifiers survive in the tree.
    #[test]
    fn r1_6_output_paths_use_deidentified_values() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        write_phi_named_input(
            &input_dir,
            "MRN0012345",
            "1.2.840.111.1",
            "1.2.840.111.2",
            "3",
            "1.2.840.111.3",
        );

        let pipeline = DeidPipeline::from_recipe_text(
            LAYOUT_RECIPE,
            layout_config(input_dir, output_dir.clone()),
        )
        .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("should run pipeline");
        assert_eq!(report.files_processed, 1);

        let written = output_tree(&output_dir);
        assert_eq!(written.len(), 1, "one file should be written");
        let relative = &written[0];

        // Four components: patient / study / series_number / instance.dcm
        assert_eq!(relative.components().count(), 4);

        // The whole point: no original identifier anywhere in the path.
        let as_text = relative.to_string_lossy();
        for phi in [
            "MRN0012345",
            "1.2.840.111.1",
            "1.2.840.111.2",
            "1.2.840.111.3",
        ] {
            assert!(
                !as_text.contains(phi),
                "output path {} must not contain the original identifier {}",
                as_text,
                phi
            );
        }

        // The path values must equal the values inside the file.
        let result = open_file(output_dir.join(relative)).expect("should open output");
        let value = |tag| {
            result
                .element(tag)
                .expect("tag present")
                .value()
                .to_str()
                .expect("readable")
                .trim()
                .to_string()
        };
        let parts: Vec<String> = relative
            .components()
            .map(|c| c.as_os_str().to_string_lossy().to_string())
            .collect();
        assert_eq!(parts[0], value(dicom_dictionary_std::tags::PATIENT_ID));
        assert_eq!(
            parts[1],
            value(dicom_dictionary_std::tags::STUDY_INSTANCE_UID)
        );
        assert_eq!(
            parts[2],
            format!(
                "{}_{}",
                value(dicom_dictionary_std::tags::SERIES_INSTANCE_UID),
                value(dicom_dictionary_std::tags::SERIES_NUMBER)
            )
        );
        assert_eq!(
            parts[3],
            format!(
                "{}.dcm",
                value(dicom_dictionary_std::tags::SOP_INSTANCE_UID)
            )
        );
    }

    /// Requirement r-1-6: files from one study group under one directory,
    /// so the de-identified tree keeps the original's shape.
    #[test]
    fn r1_6_layout_preserves_the_study_series_hierarchy() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        // Two instances of the same series — same series number, so they
        // must land in the same series directory.
        for sop in ["1.2.3.1", "1.2.3.2"] {
            write_phi_named_input(
                &input_dir,
                "MRN1",
                "1.2.840.111.1",
                "1.2.840.111.2",
                "3",
                sop,
            );
        }
        // A second patient, same study/series UIDs are not shared.
        write_phi_named_input(
            &input_dir,
            "MRN2",
            "1.2.840.222.1",
            "1.2.840.222.2",
            "1",
            "1.2.3.3",
        );

        let pipeline = DeidPipeline::from_recipe_text(
            LAYOUT_RECIPE,
            layout_config(input_dir, output_dir.clone()),
        )
        .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("should run pipeline");
        assert_eq!(report.files_processed, 3);

        let written = output_tree(&output_dir);
        assert_eq!(written.len(), 3);

        let patients: HashSet<_> = written
            .iter()
            .map(|p| p.components().next().expect("patient dir").as_os_str())
            .collect();
        assert_eq!(patients.len(), 2, "two patients, two top-level directories");

        // The two MRN1 instances share a patient and study directory.
        let studies: HashSet<_> = written
            .iter()
            .map(|p| p.parent().expect("has parent").to_path_buf())
            .collect();
        assert_eq!(studies.len(), 2, "MRN1's two instances share a series dir");
    }

    /// Requirement r-1-4: without a layout, the input tree is mirrored.
    #[test]
    fn r1_4_no_layout_still_mirrors_the_input_tree() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        let input = write_phi_named_input(
            &input_dir,
            "MRN1",
            "1.2.840.111.1",
            "1.2.840.111.2",
            "3",
            "1.2.840.111.3",
        );
        let expected = input
            .strip_prefix(&input_dir)
            .expect("under input")
            .to_path_buf();

        let mut config = layout_config(input_dir, output_dir.clone());
        config.output_layout = None;
        let pipeline =
            DeidPipeline::from_recipe_text(LAYOUT_RECIPE, config).expect("should create pipeline");
        pipeline
            .run_with_progress(|_, _, _| {})
            .expect("should run pipeline");

        assert_eq!(output_tree(&output_dir), vec![expected]);
    }

    /// Requirement r-1-8: a data set missing a layout tag is skipped per
    /// r-1-5, and the rest of the run continues.
    #[test]
    fn r1_8_missing_layout_tag_skips_only_that_file() {
        use crate::test_helpers::*;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        write_phi_named_input(&input_dir, "MRN1", "1.2.1", "1.2.2", "1", "1.2.3");

        // A second file carrying no SeriesNumber. The recipe does not
        // touch that tag, so nothing puts one back and the layout cannot
        // be rendered.
        let mut orphan = create_test_file_obj();
        put_str(
            &mut orphan,
            dicom_dictionary_std::tags::PATIENT_ID,
            VR::LO,
            "MRN2",
        );
        put_str(
            &mut orphan,
            dicom_dictionary_std::tags::STUDY_INSTANCE_UID,
            VR::UI,
            "1.2.9",
        );
        put_str(
            &mut orphan,
            dicom_dictionary_std::tags::SERIES_INSTANCE_UID,
            VR::UI,
            "1.2.10",
        );
        put_str(
            &mut orphan,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            VR::UI,
            "1.2.11",
        );
        orphan
            .write_to_file(input_dir.join("orphan.dcm"))
            .expect("write orphan");

        let pipeline = DeidPipeline::from_recipe_text(
            LAYOUT_RECIPE,
            layout_config(input_dir, output_dir.clone()),
        )
        .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("run should not abort");

        assert_eq!(report.files_processed, 1);
        assert_eq!(report.files_skipped, 1, "the orphan is skipped, not fatal");
        assert_eq!(output_tree(&output_dir).len(), 1);
    }

    /// Requirement r-1-8: a recipe that blanks a layout tag skips every
    /// file rather than writing to a degenerate path.
    #[test]
    fn r1_8_blanked_layout_tag_skips_the_file() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        write_phi_named_input(&input_dir, "MRN1", "1.2.1", "1.2.2", "1", "1.2.3");

        let recipe = "\
FORMAT dicom
%header
BLANK PatientID
REPLACE SOPInstanceUID func:hashuid
";
        let pipeline =
            DeidPipeline::from_recipe_text(recipe, layout_config(input_dir, output_dir.clone()))
                .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("run should not abort");

        assert_eq!(report.files_processed, 0);
        assert_eq!(report.files_skipped, 1);
        assert!(output_tree(&output_dir).is_empty());
    }

    /// Requirement r-1-9: two inputs that render to the same path do not
    /// silently overwrite each other.
    #[test]
    fn r1_9_output_path_collision_is_skipped_not_overwritten() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");

        // The same instance filed under two different input directories —
        // a duplicated export, which renders to one output path.
        write_phi_named_input(&input_dir.join("a"), "MRN1", "1.2.1", "1.2.2", "1", "1.2.3");
        write_phi_named_input(&input_dir.join("b"), "MRN1", "1.2.1", "1.2.2", "1", "1.2.3");

        let pipeline = DeidPipeline::from_recipe_text(
            LAYOUT_RECIPE,
            layout_config(input_dir, output_dir.clone()),
        )
        .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("run should not abort");

        assert_eq!(report.files_processed, 1);
        assert_eq!(report.files_skipped, 1, "the duplicate is skipped");
        assert_eq!(output_tree(&output_dir).len(), 1);
    }

    /// Requirement r-1-9: collisions are detected across threads too.
    #[cfg(feature = "parallel")]
    #[test]
    fn r1_9_collision_detected_in_parallel_run() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        for dir in ["a", "b", "c", "d"] {
            write_phi_named_input(&input_dir.join(dir), "MRN1", "1.2.1", "1.2.2", "1", "1.2.3");
        }

        let pipeline = DeidPipeline::from_recipe_text(
            LAYOUT_RECIPE,
            layout_config(input_dir, output_dir.clone()),
        )
        .expect("should create pipeline");
        let report = pipeline
            .run_parallel(4, |_, _, _| {})
            .expect("run should not abort");

        assert_eq!(report.files_processed, 1, "exactly one thread may claim it");
        assert_eq!(report.files_skipped, 3);
        assert_eq!(output_tree(&output_dir).len(), 1);
    }

    /// Requirement r-1-9: the same pipeline can be run twice; claimed
    /// paths from the first run do not poison the second.
    #[test]
    fn r1_9_claimed_paths_reset_between_runs() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        write_phi_named_input(&input_dir, "MRN1", "1.2.1", "1.2.2", "1", "1.2.3");

        let pipeline =
            DeidPipeline::from_recipe_text(LAYOUT_RECIPE, layout_config(input_dir, output_dir))
                .expect("should create pipeline");

        for run in 1..=2 {
            let report = pipeline
                .run_with_progress(|_, _, _| {})
                .unwrap_or_else(|e| panic!("run {} should succeed: {}", run, e));
            assert_eq!(report.files_processed, 1, "run {}", run);
            assert_eq!(report.files_skipped, 0, "run {}", run);
        }
    }

    /// Requirement r-1-10: the mapping file records original to
    /// de-identified paths and lands outside the output directory.
    #[test]
    fn r1_10_mapping_file_records_path_pairs() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        let mapping_path = tmp.path().join("keys").join("mapping.tsv");
        let input = write_phi_named_input(&input_dir, "MRN1", "1.2.1", "1.2.2", "1", "1.2.3");

        let mut config = layout_config(input_dir, output_dir.clone());
        config.mapping_file = Some(mapping_path.clone());
        let pipeline =
            DeidPipeline::from_recipe_text(LAYOUT_RECIPE, config).expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("should run pipeline");

        assert_eq!(report.mapping_file_path.as_ref(), Some(&mapping_path));
        let content = fs::read_to_string(&mapping_path).expect("read mapping");
        let lines: Vec<&str> = content.trim().lines().collect();
        assert_eq!(lines.len(), 2, "a header plus one row");
        assert_eq!(lines[0], "original_path\tdeidentified_path");

        let (original, deidentified) = lines[1].split_once('\t').expect("tab separated");
        assert_eq!(Path::new(original), input);
        let written = output_tree(&output_dir);
        assert_eq!(Path::new(deidentified), output_dir.join(&written[0]));

        // The mapping is PHI; it must not be inside the output tree.
        assert!(!mapping_path.starts_with(&output_dir));
    }

    /// Requirement r-1-10: a mapping file inside the output directory is
    /// refused before any file is processed.
    #[test]
    fn r1_10_mapping_file_inside_output_dir_is_rejected() {
        let tmp = TempDir::new().expect("should create temp dir");
        let output_dir = tmp.path().join("output");

        for candidate in [
            output_dir.join("mapping.tsv"),
            output_dir.join("sub").join("mapping.tsv"),
            output_dir.join("..").join("output").join("mapping.tsv"),
        ] {
            let mut config = layout_config(tmp.path().join("input"), output_dir.clone());
            config.mapping_file = Some(candidate.clone());
            let err = DeidPipeline::from_recipe_text(LAYOUT_RECIPE, config)
                .err()
                .unwrap_or_else(|| panic!("should reject {}", candidate.display()));
            assert!(
                err.to_string().contains("must be stored outside"),
                "unexpected error for {}: {}",
                candidate.display(),
                err
            );
        }
    }

    // -- r-1-12 --------------------------------------------------------------

    fn unprotected(recipe_text: &str, template: &str) -> Vec<Tag> {
        let recipe = Recipe::parse(recipe_text).expect("valid recipe");
        let layout = OutputLayout::parse(template).expect("valid layout");
        unprotected_layout_tags(&layout, &recipe, false)
    }

    /// Requirement r-1-12: a layout tag the recipe never changes is
    /// flagged, because its original value lands in the path.
    #[test]
    fn r1_12_flags_layout_tags_the_recipe_does_not_deidentify() {
        assert_eq!(
            unprotected(LAYOUT_RECIPE, "{PatientID}/{AccessionNumber}.dcm"),
            vec![dicom_dictionary_std::tags::ACCESSION_NUMBER]
        );
    }

    /// Requirement r-1-12: the canonical layout against a recipe that
    /// hashes every identifier must be silent — SeriesNumber's numeric
    /// VR cannot carry PHI.
    #[test]
    fn r1_12_does_not_flag_numeric_vr_tags() {
        assert!(
            unprotected(LAYOUT_RECIPE, DEID_PATH_LAYOUT).is_empty(),
            "SeriesNumber (VR IS) must not be flagged"
        );
    }

    /// Requirement r-1-12: KEEP and ADD do not change an existing value,
    /// so neither protects a layout tag.
    #[test]
    fn r1_12_keep_and_add_do_not_count_as_deidentifying() {
        let recipe = "\
FORMAT dicom
%header
KEEP PatientID
ADD StudyInstanceUID 1.2.3
REPLACE SOPInstanceUID func:hashuid
";
        assert_eq!(
            unprotected(recipe, DEID_PATH_LAYOUT),
            vec![
                dicom_dictionary_std::tags::PATIENT_ID,
                dicom_dictionary_std::tags::STUDY_INSTANCE_UID,
                dicom_dictionary_std::tags::SERIES_INSTANCE_UID,
            ]
        );
    }

    /// Requirement r-1-12: REMOVE, BLANK and JITTER all change the
    /// stored value, so they count as de-identifying.
    #[test]
    fn r1_12_remove_blank_and_jitter_count_as_deidentifying() {
        let recipe = "\
FORMAT dicom
%header
REMOVE PatientID
BLANK StudyInstanceUID
JITTER SeriesInstanceUID 5
REPLACE SOPInstanceUID func:hashuid
";
        assert!(unprotected(recipe, DEID_PATH_LAYOUT).is_empty());
    }

    /// Requirement r-1-12: a pattern action could cover anything, so the
    /// check stays silent rather than crying wolf.
    ///
    /// Patterns are only reachable through the library API (r-6-1): the
    /// recipe file syntax has no form that parses to a
    /// `TagSpecifier::Pattern`, so the recipe is built directly here.
    #[test]
    fn r1_12_pattern_actions_suppress_the_warning() {
        use crate::recipe::HeaderAction;

        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![HeaderAction {
                action_type: ActionType::Remove,
                tag: TagSpecifier::Pattern(".*UID$".into()),
                value: None,
            }],
            filters: vec![],
        };
        let layout = OutputLayout::parse(DEID_PATH_LAYOUT).expect("valid layout");
        assert!(unprotected_layout_tags(&layout, &recipe, false).is_empty());
    }

    /// Requirement r-7-8: mapper mode carries no recipe, but it does
    /// replace PatientID on every written file, so a layout reading
    /// PatientID must not raise the r-1-12 warning.
    #[test]
    fn r7_8_mapper_mode_protects_patient_id() {
        let recipe = Recipe {
            format: "dicom".into(),
            header: vec![],
            filters: vec![],
        };
        let layout = OutputLayout::parse("{PatientID}/{SOPInstanceUID}.dcm").expect("valid layout");
        assert_eq!(
            unprotected_layout_tags(&layout, &recipe, true),
            vec![dicom_dictionary_std::tags::SOP_INSTANCE_UID],
            "only the tag the mapper does not touch should be flagged"
        );
    }

    // -- r-7 -----------------------------------------------------------------

    /// Write an input file carrying the given PatientID, plus the tags a
    /// mapper-mode run must leave alone.
    fn write_mapper_input(input_dir: &Path, name: &str, patient_id: &str) -> PathBuf {
        use crate::test_helpers::*;

        let mut obj = create_test_file_obj();
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::PATIENT_ID,
            VR::LO,
            patient_id,
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::PATIENT_NAME,
            VR::PN,
            "Doe^Jane",
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            VR::UI,
            &format!("1.2.3.{}", name),
        );
        put_str(&mut obj, dicom_dictionary_std::tags::MODALITY, VR::CS, "CT");
        // A private tag, which mapper mode must not strip (r-7-2).
        put_str(&mut obj, Tag(0x0009, 0x0010), VR::LO, "ACME");

        fs::create_dir_all(input_dir).expect("create input dir");
        let path = input_dir.join(format!("{}.dcm", name));
        obj.write_to_file(&path).expect("write input file");
        path
    }

    fn mapper_config(input_dir: PathBuf, output_dir: PathBuf) -> DeidConfig {
        DeidConfig {
            input_dir,
            output_dir,
            recipe_path: PathBuf::new(),
            variables: HashMap::new(),
            functions: HashMap::new(),
            salt: None,
            output_layout: None,
            mapping_file: None,
        }
    }

    /// Requirement r-7-2: a full run replaces PatientID and nothing else.
    #[test]
    fn r7_2_mapper_run_changes_only_the_patient_id() {
        use crate::mapper::PatientIdMapper;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        write_mapper_input(&input_dir, "a", "MRN001");

        let mapper =
            PatientIdMapper::from_pairs([("MRN001", "ANON-1")]).expect("should build mapper");
        let pipeline =
            DeidPipeline::from_mapper(mapper, mapper_config(input_dir, output_dir.clone()))
                .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("should run pipeline");

        assert_eq!(report.files_processed, 1);
        assert_eq!(report.files_skipped, 0);
        assert_eq!(report.files_blacklisted, 0);

        let result = open_file(output_dir.join("a.dcm")).expect("should open output");
        let value = |tag| {
            result
                .element(tag)
                .unwrap_or_else(|_| panic!("{:?} should be present", tag))
                .value()
                .to_str()
                .expect("readable")
                .trim()
                .to_string()
        };
        assert_eq!(value(dicom_dictionary_std::tags::PATIENT_ID), "ANON-1");
        assert_eq!(
            value(dicom_dictionary_std::tags::PATIENT_NAME),
            "Doe^Jane",
            "no recipe ran, so PatientName must survive"
        );
        assert_eq!(
            value(Tag(0x0009, 0x0010)),
            "ACME",
            "mapper mode must not remove private tags"
        );
        // r-3-14 still applies: the meta group is synced regardless.
        assert_eq!(
            result.meta().media_storage_sop_instance_uid(),
            value(dicom_dictionary_std::tags::SOP_INSTANCE_UID)
        );
    }

    /// Requirement r-7-6: an unmapped file is skipped and never written,
    /// while the rest of the run continues.
    #[test]
    fn r7_6_unmapped_file_is_skipped_and_the_run_continues() {
        use crate::mapper::PatientIdMapper;

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        write_mapper_input(&input_dir, "a", "MRN001");
        write_mapper_input(&input_dir, "b", "MRN999");

        let mapper =
            PatientIdMapper::from_pairs([("MRN001", "ANON-1")]).expect("should build mapper");
        let pipeline =
            DeidPipeline::from_mapper(mapper, mapper_config(input_dir, output_dir.clone()))
                .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("run should not abort");

        assert_eq!(report.files_processed, 1);
        assert_eq!(report.files_skipped, 1);
        assert!(output_dir.join("a.dcm").exists());
        assert!(
            !output_dir.join("b.dcm").exists(),
            "an unmapped PatientID must never be written to the output"
        );
    }

    /// Requirement r-7-2: Implicit VR Little Endian is what most
    /// archives actually store, and mapper mode never decompresses, so
    /// the file must be read, mapped, and written back in that same
    /// transfer syntax. The VR is not carried in the stream at all
    /// there, so the replacement has to survive a round trip that
    /// re-derives it from the dictionary.
    #[test]
    fn r7_2_maps_an_implicit_vr_little_endian_file() {
        use crate::mapper::PatientIdMapper;
        use crate::test_helpers::put_str;
        use dicom_object::meta::FileMetaTableBuilder;

        const IMPLICIT_VR_LE: &str = "1.2.840.10008.1.2";

        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        fs::create_dir_all(&input_dir).expect("create input dir");

        let mut obj = dicom_object::FileDicomObject::new_empty_with_meta(
            FileMetaTableBuilder::new()
                .transfer_syntax(IMPLICIT_VR_LE)
                .media_storage_sop_class_uid("1.2.840.10008.5.1.4.1.1.2")
                .media_storage_sop_instance_uid("1.2.3.4.5")
                .implementation_class_uid("1.2.3.4")
                .build()
                .expect("valid file meta"),
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::PATIENT_ID,
            VR::LO,
            "MRN001",
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::PATIENT_NAME,
            VR::PN,
            "Doe^Jane",
        );
        put_str(
            &mut obj,
            dicom_dictionary_std::tags::SOP_INSTANCE_UID,
            VR::UI,
            "1.2.3.4.5",
        );
        obj.write_to_file(input_dir.join("implicit.dcm"))
            .expect("write implicit VR input");

        let mapper =
            PatientIdMapper::from_pairs([("MRN001", "ANON-1")]).expect("should build mapper");
        let pipeline =
            DeidPipeline::from_mapper(mapper, mapper_config(input_dir, output_dir.clone()))
                .expect("should create pipeline");
        let report = pipeline
            .run_with_progress(|_, _, _| {})
            .expect("should run pipeline");
        assert_eq!(report.files_processed, 1);
        assert_eq!(report.files_skipped, 0);

        let result = open_file(output_dir.join("implicit.dcm")).expect("should open output");
        assert_eq!(
            result.meta().transfer_syntax(),
            IMPLICIT_VR_LE,
            "mapper mode decompresses nothing, so the transfer syntax must be preserved"
        );
        let value = |tag| {
            result
                .element(tag)
                .expect("tag present")
                .value()
                .to_str()
                .expect("readable")
                .trim()
                .to_string()
        };
        assert_eq!(value(dicom_dictionary_std::tags::PATIENT_ID), "ANON-1");
        assert_eq!(value(dicom_dictionary_std::tags::PATIENT_NAME), "Doe^Jane");
    }

    /// Requirement r-7-1: the mapper file is read and validated when the
    /// pipeline is built, before any DICOM file is processed.
    #[test]
    fn r7_1_mapper_file_is_validated_at_construction() {
        let tmp = TempDir::new().expect("should create temp dir");
        let input_dir = tmp.path().join("input");
        let output_dir = tmp.path().join("output");
        write_mapper_input(&input_dir, "a", "MRN001");

        let mapper_path = tmp.path().join("ids.csv");
        fs::write(&mapper_path, "PatientID,DeidPatientID\nMRN001,ANON-1\n").expect("write mapper");
        let pipeline = DeidPipeline::from_mapper_file(
            &mapper_path,
            mapper_config(input_dir.clone(), output_dir.clone()),
        )
        .expect("should load the mapper");
        assert_eq!(
            pipeline
                .run_with_progress(|_, _, _| {})
                .expect("should run")
                .files_processed,
            1
        );

        // A mapper with conflicting duplicates fails before the run.
        let bad_path = tmp.path().join("bad.csv");
        fs::write(&bad_path, "MRN001,ANON-1\nMRN001,ANON-2\n").expect("write mapper");
        let err = DeidPipeline::from_mapper_file(&bad_path, mapper_config(input_dir, output_dir))
            .err()
            .expect("should reject the mapper");
        assert!(
            err.to_string().contains("already mapped"),
            "unexpected error: {}",
            err
        );
    }

    /// Requirement r-1-6: a bad layout template fails at construction,
    /// before any file is touched.
    #[test]
    fn r1_6_invalid_layout_fails_at_construction() {
        let tmp = TempDir::new().expect("should create temp dir");
        let mut config = layout_config(tmp.path().join("input"), tmp.path().join("output"));
        config.output_layout = Some("{NoSuchTag}/{SOPInstanceUID}.dcm".into());
        let err = DeidPipeline::from_recipe_text(LAYOUT_RECIPE, config)
            .err()
            .expect("should reject the template");
        assert!(err.to_string().contains("unknown tag keyword"));
    }
}
