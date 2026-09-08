//! Dependency-list parsing and input discovery for C and C++ compiles.

use crate::{
    CcActionContext, CcActionInput, CcBypassReason, CcCompilerFamily, MAX_INPUT_BYTES,
    MAX_MANIFEST_ENTRIES, MAX_PREDICTED_INPUTS, normalize_components,
};
use mbx_cache_core::{
    CacheDigest, FileDigestCache, FileDigestScope, FileIdentity, FileObservation,
    FileObservationMatch, FileObservationResolution, FileSnapshot,
};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Marker distinguishing an include-directory name manifest from a file input.
pub const INCLUDE_MANIFEST_PREFIX: &str = "@include-manifest:";

/// Directives whose operands are consumed by the assembler, after the
/// compiler has finished producing its dependency list.
const ASSEMBLER_INPUT_DIRECTIVES: &[&[u8]] = &[b".include", b".incbin", b".sinclude"];

const SCAN_CHUNK_BYTES: usize = 64 * 1024;

fn observation_error(paths: &[PathBuf], error: std::io::Error) -> CcBypassReason {
    // The shared batch API intentionally aborts the whole batch when one file
    // cannot be stabilized, but currently exposes only WouldBlock rather than
    // the offending path. Keep the path-bearing adapter error until the core
    // API grows a structured capture error; callers still treat it as a
    // routine cache bypass and continue with the compiler.
    CcBypassReason::InputRead {
        path: paths
            .iter()
            .find(|path| {
                std::fs::metadata(path)
                    .map(|metadata| !metadata.is_file())
                    .unwrap_or(true)
            })
            .or_else(|| paths.first())
            .cloned()
            .unwrap_or_default(),
        message: error.to_string(),
    }
}

/// A parsed GNU-style dependency list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CcDepfile {
    /// Prerequisite files named by the first rule.
    pub files: Vec<PathBuf>,
}

impl CcDepfile {
    /// Read and parse the dependency list the compiler wrote.
    pub fn read(path: &Path) -> Result<Self, CcBypassReason> {
        let contents =
            std::fs::read_to_string(path).map_err(|error| CcBypassReason::DepfileRead {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        Self::parse(&contents)
    }

    /// Read the dependency output emitted by the selected compiler family.
    pub fn read_for(path: &Path, family: CcCompilerFamily) -> Result<Self, CcBypassReason> {
        if family.is_msvc() {
            Self::read_msvc(path)
        } else {
            Self::read(path)
        }
    }

    /// Read MSVC's `/sourceDependencies` JSON output.
    pub fn read_msvc(path: &Path) -> Result<Self, CcBypassReason> {
        let contents = std::fs::read(path).map_err(|error| CcBypassReason::DepfileRead {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        let value: serde_json::Value = serde_json::from_slice(&contents)
            .map_err(|error| CcBypassReason::MalformedDepfile(error.to_string()))?;
        let data = value
            .get("Data")
            .and_then(serde_json::Value::as_object)
            .ok_or_else(|| CcBypassReason::MalformedDepfile("missing Data object".into()))?;
        if data
            .get("ImportedModules")
            .and_then(serde_json::Value::as_array)
            .is_some_and(|modules| !modules.is_empty())
            || data.get("ProvidedModule").is_some_and(|module| {
                !module.is_null() && module.as_str().is_none_or(|s| !s.is_empty())
            })
        {
            return Err(CcBypassReason::MalformedDepfile(
                "C++ module dependencies are not modeled".into(),
            ));
        }
        let includes = data
            .get("Includes")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| CcBypassReason::MalformedDepfile("missing Includes array".into()))?;
        let files = includes
            .iter()
            .map(|entry| {
                entry.as_str().map(PathBuf::from).ok_or_else(|| {
                    CcBypassReason::MalformedDepfile("non-string include path".into())
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        Ok(Self { files })
    }

    /// Parse a GNU-style dependency list.
    ///
    /// Only the first rule is read. The adapter never passes `-MP`, so a
    /// well-formed file the adapter asked for has exactly one rule, and
    /// anything further is ignored rather than guessed at.
    pub fn parse(contents: &str) -> Result<Self, CcBypassReason> {
        let joined = join_continuations(contents)?;
        let (_, prerequisites) = joined
            .lines()
            .find_map(|line| line.split_once(RULE_SEPARATOR))
            .ok_or_else(|| CcBypassReason::MalformedDepfile("no dependency rule".into()))?;
        let files = split_prerequisites(prerequisites)?;
        Ok(Self { files })
    }
}

const RULE_SEPARATOR: &str = ": ";

impl CcDepfile {
    /// Render the dependency list a caller asked for, the way the driver
    /// writes one: the rule's targets, then every prerequisite on its own
    /// continued line, then, for `-MP`, an empty rule per header so make does
    /// not fail when one is deleted. `source` is the file that was compiled
    /// and gets no phony rule, exactly as the driver leaves it out.
    ///
    /// Escapes are the ones `parse` reads back: a space, a `#`, and a `$` in
    /// a path. A quoted target gets the same treatment; a literal one is
    /// written as given, which is what `-MT` promises.
    pub fn render(
        targets: &[crate::DepfileTarget],
        files: &[PathBuf],
        source: &Path,
        phony_targets: bool,
    ) -> String {
        let mut rendered = String::new();
        for (index, target) in targets.iter().enumerate() {
            if index > 0 {
                rendered.push(' ');
            }
            if target.quoted {
                rendered.push_str(&escape_make_word(&target.name));
            } else {
                rendered.push_str(&target.name);
            }
        }
        rendered.push(':');
        for file in files {
            rendered.push_str(" \\\n ");
            rendered.push_str(&escape_make_word(&file.to_string_lossy()));
        }
        rendered.push('\n');
        if phony_targets {
            for file in files.iter().filter(|file| file.as_path() != source) {
                rendered.push_str(&escape_make_word(&file.to_string_lossy()));
                rendered.push_str(":\n");
            }
        }
        rendered
    }
}

/// Quote a word for make the way the driver does in a dependency list.
fn escape_make_word(word: &str) -> String {
    let mut escaped = String::with_capacity(word.len());
    for character in word.chars() {
        match character {
            ' ' => escaped.push_str("\\ "),
            '#' => escaped.push_str("\\#"),
            '$' => escaped.push_str("$$"),
            other => escaped.push(other),
        }
    }
    escaped
}

/// Join physical lines the compiler wrapped with a trailing backslash.
fn join_continuations(contents: &str) -> Result<String, CcBypassReason> {
    let mut joined = String::with_capacity(contents.len());
    let mut continued = false;
    for line in contents.lines() {
        let trimmed = line.strip_suffix('\r').unwrap_or(line);
        let (text, continues) = match trimmed.strip_suffix('\\') {
            Some(text) => (text, true),
            None => (trimmed, false),
        };
        if continued {
            joined.push(' ');
        }
        joined.push_str(text.trim_end_matches(['\t']));
        if !continues {
            joined.push('\n');
        }
        continued = continues;
    }
    if continued {
        return Err(CcBypassReason::MalformedDepfile(
            "unterminated line continuation".into(),
        ));
    }
    Ok(joined)
}

/// Split a prerequisite list, honoring exactly the escapes make defines.
///
/// Anything else escaped is a spelling this parser does not model, and a
/// mis-parsed prerequisite would silently drop an input from the key.
fn split_prerequisites(value: &str) -> Result<Vec<PathBuf>, CcBypassReason> {
    let mut files = Vec::new();
    let mut current = String::new();
    let mut characters = value.chars().peekable();
    while let Some(character) = characters.next() {
        match character {
            ' ' | '\t' => {
                if !current.is_empty() {
                    files.push(PathBuf::from(std::mem::take(&mut current)));
                }
            }
            '\\' => match characters.next() {
                Some(' ') => current.push(' '),
                Some('#') => current.push('#'),
                Some(other) => {
                    return Err(CcBypassReason::MalformedDepfile(format!(
                        "unmodeled escape \\{other}"
                    )));
                }
                None => {
                    return Err(CcBypassReason::MalformedDepfile(
                        "trailing escape character".into(),
                    ));
                }
            },
            '$' => match characters.next() {
                Some('$') => current.push('$'),
                Some(other) => {
                    return Err(CcBypassReason::MalformedDepfile(format!(
                        "unmodeled variable reference ${other}"
                    )));
                }
                None => {
                    return Err(CcBypassReason::MalformedDepfile(
                        "trailing variable reference".into(),
                    ));
                }
            },
            other => current.push(other),
        }
    }
    if !current.is_empty() {
        files.push(PathBuf::from(current));
    }
    Ok(files)
}

/// A complete, content-addressed compiler input manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CcDiscoveredInputs {
    working_dir: PathBuf,
    /// Content-addressed inputs, including include-directory manifests.
    pub inputs: Vec<CcActionInput>,
    /// What each file input looked like on disk when its digest was
    /// established, index-aligned with `inputs`; `None` for manifests and
    /// where the filesystem gave nothing to compare against later.
    observations: Vec<Option<FileObservation>>,
}

impl CcDiscoveredInputs {
    /// Digest every file the compilation read, and summarize the directories it
    /// searched.
    ///
    /// Digesting the files answers "did any input change". The directory
    /// manifests answer the question a dependency list cannot: whether a header
    /// that was *not* read now exists somewhere that would shadow one that was.
    pub fn collect(
        working_dir: &Path,
        files: BTreeSet<PathBuf>,
        directories: BTreeSet<PathBuf>,
        digests: &dyn FileDigestCache,
    ) -> Result<Self, CcBypassReason> {
        if !working_dir.is_absolute() {
            return Err(CcBypassReason::RelativeWorkingDirectory(
                working_dir.to_path_buf(),
            ));
        }
        let directories = minimal_manifest_directories(directories);
        if files.len() + directories.len() > MAX_PREDICTED_INPUTS {
            return Err(CcBypassReason::TooManyInputs);
        }
        let working_dir = normalize_components(working_dir);
        let file_paths = files.into_iter().collect::<Vec<_>>();
        let mut inputs = Vec::with_capacity(file_paths.len() + directories.len());
        let mut total_bytes = 0_u64;
        // Preserve the adapter's byte budget before asking the observer to
        // read anything. The observer repeats the metadata check because its
        // identity must be current; this pass exists only to bound work.
        for path in &file_paths {
            let metadata = std::fs::metadata(path).map_err(|error| CcBypassReason::InputRead {
                path: path.clone(),
                message: error.to_string(),
            })?;
            if !metadata.is_file() {
                return Err(CcBypassReason::InputRead {
                    path: path.clone(),
                    message: "input is not a regular file".into(),
                });
            }
            total_bytes = total_bytes.saturating_add(metadata.len());
            if total_bytes > MAX_INPUT_BYTES {
                return Err(CcBypassReason::TooManyInputs);
            }
        }
        // The shared observer owns metadata qualification, regular-file
        // checks, and the batched cache lookup. It performs one validated read
        // for every miss, including the timestamp-macro scan.
        let resolutions = FileObservation::capture_many_with_scope(
            FileDigestScope::CcInput,
            file_paths.iter().map(PathBuf::as_path),
            digests,
        )
        .map_err(|error| observation_error(&file_paths, error))?;
        let mut observations = Vec::with_capacity(inputs.capacity());
        for (path, resolution) in file_paths.into_iter().zip(resolutions) {
            let observation = match resolution {
                Some(FileObservationResolution::Observation(observation)) => observation,
                Some(FileObservationResolution::EmbeddedTimestampMacro) => {
                    return Err(CcBypassReason::EmbeddedTimestampMacro(path));
                }
                None => {
                    return Err(CcBypassReason::InputRead {
                        path,
                        message: "could not establish a file observation".into(),
                    });
                }
            };
            inputs.push(CcActionInput {
                path,
                digest: observation.digest.clone(),
            });
            observations.push(Some(observation));
        }
        let mut manifest_entries = 0_usize;
        for directory in directories {
            let digest = include_manifest(&directory, &mut manifest_entries)?;
            inputs.push(CcActionInput {
                path: PathBuf::from(format!("{INCLUDE_MANIFEST_PREFIX}{}", directory.display())),
                digest,
            });
            observations.push(None);
        }
        Ok(Self {
            working_dir,
            inputs,
            observations,
        })
    }

    /// File inputs, excluding include-directory manifests.
    pub fn files(&self) -> impl Iterator<Item = &CcActionInput> {
        self.inputs
            .iter()
            .filter(|input| !is_manifest_input(&input.path))
    }

    /// Reject inputs whose modification time overlaps the compiler invocation.
    ///
    /// Inputs known before the compiler ran are compared by their complete
    /// content observations. Newly discovered headers retain the timestamp
    /// barrier because no pre-compilation observation exists for them.
    pub fn verify_not_modified_since(&self, started_at: SystemTime) -> Result<(), CcBypassReason> {
        self.verify_not_modified_since_with_observations(started_at, &BTreeMap::new())
    }

    /// Reject inputs that changed from observations captured before the driver
    /// ran, falling back to the wall-clock barrier for discovered headers.
    pub fn verify_not_modified_since_with_observations(
        &self,
        started_at: SystemTime,
        before: &BTreeMap<PathBuf, FileObservation>,
    ) -> Result<(), CcBypassReason> {
        for (index, input) in self.inputs.iter().enumerate() {
            if is_manifest_input(&input.path) {
                continue;
            }
            let Some(observation) = self.observations.get(index).and_then(Option::as_ref) else {
                return Err(CcBypassReason::InputModifiedDuringCompilation(
                    input.path.clone(),
                ));
            };
            if let Some(previous) = before.get(&input.path) {
                match previous.compare(Some(&observation.identity), &observation.digest) {
                    FileObservationMatch::Reusable => continue,
                    FileObservationMatch::Changed => {
                        return Err(CcBypassReason::InputChanged(input.path.clone()));
                    }
                    FileObservationMatch::Indeterminate => {
                        return Err(CcBypassReason::InputModifiedDuringCompilation(
                            input.path.clone(),
                        ));
                    }
                }
            }
            let modified = std::fs::metadata(&input.path)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| CcBypassReason::InputRead {
                    path: input.path.clone(),
                    message: error.to_string(),
                })?;
            if modified >= started_at {
                return Err(CcBypassReason::InputModifiedDuringCompilation(
                    input.path.clone(),
                ));
            }
        }
        Ok(())
    }

    /// Compatibility form for callers using the earlier snapshot API.
    pub fn verify_not_modified_since_with_snapshots(
        &self,
        started_at: SystemTime,
        before: &BTreeMap<PathBuf, FileSnapshot>,
    ) -> Result<(), CcBypassReason> {
        for (index, input) in self.inputs.iter().enumerate() {
            if is_manifest_input(&input.path) {
                continue;
            }
            if let Some(previous) = before.get(&input.path)
                && previous.proves_content_change()
            {
                let Some(current) = self.observations.get(index).and_then(Option::as_ref) else {
                    return Err(CcBypassReason::InputModifiedDuringCompilation(
                        input.path.clone(),
                    ));
                };
                if previous.matches(Some(&current.identity), &current.digest) {
                    continue;
                }
                return Err(CcBypassReason::InputModifiedDuringCompilation(
                    input.path.clone(),
                ));
            }
            let modified = std::fs::metadata(&input.path)
                .and_then(|metadata| metadata.modified())
                .map_err(|error| CcBypassReason::InputRead {
                    path: input.path.clone(),
                    message: error.to_string(),
                })?;
            if modified >= started_at {
                return Err(CcBypassReason::InputModifiedDuringCompilation(
                    input.path.clone(),
                ));
            }
        }
        Ok(())
    }

    /// Compatibility form for callers that captured metadata identities.
    pub fn verify_not_modified_since_with_identities(
        &self,
        started_at: SystemTime,
        before: &BTreeMap<PathBuf, FileIdentity>,
    ) -> Result<(), CcBypassReason> {
        let snapshots = before
            .iter()
            .map(|(path, identity)| (path.clone(), identity.clone().into()))
            .collect();
        self.verify_not_modified_since_with_snapshots(started_at, &snapshots)
    }

    /// Confirm every discovered file before publication, degrading a changed
    /// input to a miss rather than storing an object under a stale key.
    ///
    /// A successful comparison means both observations have the same content
    /// and a compatible file object. Timestamp churn alone is tolerated where
    /// the filesystem exposes an object identity; replacement or an unknown
    /// object is conservatively treated as indeterminate.
    pub fn verify(&self) -> Result<(), CcBypassReason> {
        self.verify_with_cache(&mbx_cache_core::NoFileDigestCache)
    }

    /// Confirm every discovered file while reusing validated session digests.
    pub fn verify_with_cache(&self, digests: &dyn FileDigestCache) -> Result<(), CcBypassReason> {
        let mut current = vec![None; self.inputs.len()];
        let mut pending = Vec::new();
        for (index, input) in self.inputs.iter().enumerate() {
            if is_manifest_input(&input.path) {
                continue;
            }
            let Some(previous) = self.observations.get(index).and_then(Option::as_ref) else {
                return Err(CcBypassReason::InputModifiedDuringCompilation(
                    input.path.clone(),
                ));
            };

            if previous.identity.can_skip_content_verification() {
                match previous.identity.still_describes() {
                    Ok(true) => {
                        current[index] = Some(previous.clone());
                        continue;
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return Err(CcBypassReason::InputRead {
                            path: input.path.clone(),
                            message: error.to_string(),
                        });
                    }
                }
            }
            pending.push((index, input.path.as_path()));
        }

        let pending_paths = pending
            .iter()
            .map(|(_, path)| (*path).to_path_buf())
            .collect::<Vec<_>>();
        let captured = FileObservation::capture_many_with_scope(
            FileDigestScope::CcInput,
            pending_paths.iter().map(PathBuf::as_path),
            digests,
        )
        .map_err(|error| observation_error(&pending_paths, error))?;
        for ((index, _), resolution) in pending.into_iter().zip(captured) {
            current[index] = Some(match resolution {
                Some(FileObservationResolution::Observation(observation)) => observation,
                Some(FileObservationResolution::EmbeddedTimestampMacro) => {
                    return Err(CcBypassReason::EmbeddedTimestampMacro(
                        self.inputs[index].path.clone(),
                    ));
                }
                None => {
                    return Err(CcBypassReason::InputModifiedDuringCompilation(
                        self.inputs[index].path.clone(),
                    ));
                }
            });
        }

        for (index, (input, current)) in self.inputs.iter().zip(current).enumerate() {
            if is_manifest_input(&input.path) {
                continue;
            }
            let current = current.expect("file inputs have aligned observations");
            let previous = self.observations[index]
                .as_ref()
                .expect("file inputs have aligned observations");
            match previous.compare(Some(&current.identity), &current.digest) {
                FileObservationMatch::Reusable => {}
                FileObservationMatch::Changed => {
                    return Err(CcBypassReason::InputChanged(input.path.clone()));
                }
                FileObservationMatch::Indeterminate => {
                    return Err(CcBypassReason::InputModifiedDuringCompilation(
                        input.path.clone(),
                    ));
                }
            }
        }
        Ok(())
    }

    /// Merge the manifest into an action context after confirming both use the
    /// same compiler working directory.
    pub fn apply_to(self, context: &mut CcActionContext) -> Result<(), CcBypassReason> {
        if normalize_components(&context.working_dir) != self.working_dir {
            return Err(CcBypassReason::DiscoveryWorkingDirectory);
        }
        context.inputs.extend(self.inputs);
        Ok(())
    }
}

/// Drop include directories already covered by an ancestor's recursive manifest.
///
/// Discovered headers often contribute hundreds of nested parent directories,
/// especially for amalgamated C sources. Keeping both an ancestor and its
/// descendants walks and hashes the same subtree repeatedly, and can exhaust
/// the manifest-entry budget even though the ancestor already names every
/// includable file below it.
fn minimal_manifest_directories(directories: BTreeSet<PathBuf>) -> Vec<PathBuf> {
    let mut directories = directories
        .into_iter()
        .map(|directory| {
            let normalized = normalize_components(&directory);
            (directory, normalized)
        })
        .collect::<Vec<_>>();
    directories.sort_by(|(left, left_normalized), (right, right_normalized)| {
        left_normalized
            .components()
            .count()
            .cmp(&right_normalized.components().count())
            .then_with(|| left_normalized.cmp(right_normalized))
            .then_with(|| left.cmp(right))
    });

    let mut minimal = Vec::<(PathBuf, PathBuf)>::new();
    for (directory, normalized) in directories {
        if !minimal
            .iter()
            .any(|(_, ancestor)| manifest_covers(ancestor, &normalized))
        {
            minimal.push((directory, normalized));
        }
    }
    minimal
        .into_iter()
        .map(|(directory, _)| directory)
        .collect()
}

/// Whether walking `ancestor` recursively is guaranteed to visit `descendant`.
///
/// Component-aware normalization rejects a lexical prefix that escapes through
/// `..`. Directory symlinks need an explicit check because `read_dir` follows
/// the directory it starts at but the recursive walk deliberately does not
/// follow symlink entries beneath it.
fn manifest_covers(ancestor: &Path, descendant: &Path) -> bool {
    let Ok(relative) = descendant.strip_prefix(ancestor) else {
        return false;
    };
    if relative.as_os_str().is_empty() {
        return false;
    }
    let mut current = ancestor.to_path_buf();
    for component in relative.components() {
        current.push(component);
        let Ok(metadata) = std::fs::symlink_metadata(&current) else {
            return false;
        };
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return false;
        }
    }
    true
}

fn is_manifest_input(path: &Path) -> bool {
    path.to_str()
        .is_some_and(|path| path.starts_with(INCLUDE_MANIFEST_PREFIX))
}

/// Digest the includable names in each directory, reading no file contents.
///
/// Taken once before the compiler runs and again before publishing, this is
/// what detects a header that appeared in a search directory *while* the
/// compilation was in flight. The manifest recorded in the key is the one from
/// after the compile, and without this check that later state would be claimed
/// as the state the compiler saw.
pub fn manifest_snapshot(
    directories: &BTreeSet<PathBuf>,
) -> Result<BTreeMap<PathBuf, CacheDigest>, CcBypassReason> {
    let mut budget = 0_usize;
    minimal_manifest_directories(directories.iter().cloned().collect())
        .into_iter()
        .map(|directory| {
            include_manifest(&directory, &mut budget).map(|digest| (directory, digest))
        })
        .collect()
}

/// Extensions a file must carry to be a plausible `#include` target.
///
/// An extensionless name also qualifies: C++ standard headers are spelled that
/// way and projects ship their own.
///
/// `gch` and `pch` are here because a precompiled header answers an `#include`
/// without being named by one. GCC prefers `foo.h.gch` over `foo.h` on its own,
/// with nothing on the command line to say so, which is precisely the
/// substitution these manifests exist to notice -- and the one case the
/// adapter's explicit precompiled-header bypass cannot see.
const INCLUDABLE_EXTENSIONS: &[&str] = &[
    "c", "c++", "cc", "cpp", "cxx", "def", "gch", "h", "h++", "hh", "hpp", "hxx", "inc", "inl",
    "ipp", "pch", "s", "tcc",
];

/// Whether a file name could be what an `#include` directive names.
///
/// The manifest exists to notice a file appearing where it would shadow a
/// header that was read. A build writes its own objects, dependency files, and
/// archives into these directories -- often the very directory a generated
/// header lives in -- and none of those can shadow an include. Counting them
/// would make the key depend on how many sibling compilations had finished,
/// which is not a property of this compilation at all.
fn is_includable(name: &str) -> bool {
    match name.rsplit_once('.') {
        Some((stem, extension)) if !stem.is_empty() => INCLUDABLE_EXTENSIONS
            .binary_search(&extension.to_ascii_lowercase().as_str())
            .is_ok(),
        // No extension, or a leading-dot name like `.keep`.
        _ => !name.starts_with('.'),
    }
}

/// Digest the sorted includable file names beneath a directory.
///
/// Names only: the contents of anything actually read are digested as inputs,
/// so this exists purely to notice a file appearing where it could shadow one
/// of them. A directory that does not exist has an empty manifest, which is
/// what makes "the directory was created" a key change rather than an error.
fn include_manifest(directory: &Path, budget: &mut usize) -> Result<CacheDigest, CcBypassReason> {
    include_manifest_with(directory, budget, &|path| std::fs::read_dir(path))
}

fn include_manifest_with(
    directory: &Path,
    budget: &mut usize,
    read_dir: &impl Fn(&Path) -> std::io::Result<std::fs::ReadDir>,
) -> Result<CacheDigest, CcBypassReason> {
    #[cfg(unix)]
    if let Some(memo) = manifest_memo::find(directory) {
        *budget = budget.saturating_add(memo.entries);
        if *budget > MAX_MANIFEST_ENTRIES {
            return Err(CcBypassReason::TooManyInputs);
        }
        return Ok(memo.digest);
    }
    #[cfg(unix)]
    let mut directories = Vec::new();
    #[cfg(unix)]
    let initial_budget = *budget;
    let mut names = Vec::new();
    let mut pending = vec![(directory.to_path_buf(), String::new())];
    while let Some((current, prefix)) = pending.pop() {
        // Capture before enumeration and validate again after the complete
        // walk. A directory changed while being read cannot seed the memo.
        #[cfg(unix)]
        directories.push((current.clone(), manifest_memo::Identity::read(&current)));
        let entries = match read_dir(&current) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(CcBypassReason::InputRead {
                    path: current,
                    message: error.to_string(),
                });
            }
        };
        for entry in entries {
            let entry = entry.map_err(|error| CcBypassReason::InputRead {
                path: current.clone(),
                message: error.to_string(),
            })?;
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                return Err(CcBypassReason::NonUtf8Path(entry.path()));
            };
            let relative = if prefix.is_empty() {
                name.to_string()
            } else {
                format!("{prefix}/{name}")
            };
            let file_type = entry
                .file_type()
                .map_err(|error| CcBypassReason::InputRead {
                    path: entry.path(),
                    message: error.to_string(),
                })?;
            if file_type.is_dir() {
                pending.push((entry.path(), relative));
                continue;
            }
            if !is_includable(name) {
                continue;
            }
            *budget += 1;
            if *budget > MAX_MANIFEST_ENTRIES {
                return Err(CcBypassReason::TooManyInputs);
            }
            names.push(relative);
        }
    }
    names.sort();
    let digest = CacheDigest::blake3(names.join("\n").as_bytes());
    #[cfg(unix)]
    manifest_memo::record(directory, &digest, *budget - initial_budget, directories);
    Ok(digest)
}

// A wrapper checks the same manifests during prediction, discovery, and
// publication. Reuse their exact name digest while every directory is unchanged.
// This is process-local: no persisted timestamps can outlive a filesystem mount.
#[cfg(unix)]
mod manifest_memo {
    use super::*;
    use std::os::unix::fs::MetadataExt;
    use std::sync::{Mutex, OnceLock};

    const MAX_ROOTS: usize = 32;
    const MAX_DIRECTORIES: usize = 4096;
    static MEMOS: OnceLock<Mutex<BTreeMap<PathBuf, Memo>>> = OnceLock::new();

    #[derive(Clone, PartialEq, Eq)]
    pub(super) struct Identity {
        device: u64,
        inode: u64,
        modified: (i64, i64),
        changed: (i64, i64),
        mode: u32,
    }
    impl Identity {
        pub(super) fn read(path: &Path) -> Option<Self> {
            let metadata = std::fs::metadata(path).ok()?;
            // Whole-second timestamps cannot distinguish rapid edits. Preserve
            // enumeration on filesystems that expose only that precision.
            if !metadata.is_dir() || metadata.mtime_nsec() == 0 || metadata.ctime_nsec() == 0 {
                return None;
            }
            // Reuse the digest ledger's filesystem qualification. In particular,
            // Linux NFS identities omit ctime and must keep enumerating names.
            let changed = FileIdentity::for_digest_cache(path, &metadata)
                .ok()??
                .changed?;
            Some(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
                modified: (metadata.mtime(), metadata.mtime_nsec()),
                changed,
                mode: metadata.mode(),
            })
        }
    }
    #[derive(Clone)]
    pub(super) struct Memo {
        pub digest: CacheDigest,
        pub entries: usize,
        directories: Vec<(PathBuf, Identity)>,
    }
    impl Memo {
        fn valid(&self) -> bool {
            self.directories
                .iter()
                .all(|(path, identity)| Identity::read(path).as_ref() == Some(identity))
        }
    }
    pub(super) fn find(directory: &Path) -> Option<Memo> {
        let memo = MEMOS.get()?.lock().ok()?.get(directory)?.clone();
        memo.valid().then_some(memo)
    }
    pub(super) fn record(
        directory: &Path,
        digest: &CacheDigest,
        entries: usize,
        directories: Vec<(PathBuf, Option<Identity>)>,
    ) {
        if directories.is_empty() || directories.len() > MAX_DIRECTORIES {
            return;
        }
        let Some(directories) = directories
            .into_iter()
            .map(|(path, identity)| Some((path, identity?)))
            .collect::<Option<Vec<_>>>()
        else {
            return;
        };
        let memo = Memo {
            digest: digest.clone(),
            entries,
            directories,
        };
        if !memo.valid() {
            return;
        }
        let Ok(mut memos) = MEMOS.get_or_init(Default::default).lock() else {
            return;
        };
        if memos.len() >= MAX_ROOTS {
            memos.clear();
        }
        memos.insert(directory.to_path_buf(), memo);
    }
}

/// Whether a preprocessor input can make the assembler read another file.
///
/// Searching for the directive text, including in comments and inactive
/// conditional branches, deliberately accepts false positives. Missing a real
/// directive would publish an object whose complete inputs are absent from the
/// key; bypassing an otherwise cacheable object is the safe outcome instead.
pub(crate) fn contains_assembler_input_directive(path: &Path) -> Result<bool, CcBypassReason> {
    contains_any(path, ASSEMBLER_INPUT_DIRECTIVES)
}

fn contains_any(path: &Path, needles: &[&[u8]]) -> Result<bool, CcBypassReason> {
    let file = std::fs::File::open(path).map_err(|error| CcBypassReason::InputRead {
        path: path.to_path_buf(),
        message: error.to_string(),
    })?;
    let longest = needles
        .iter()
        .map(|needle| needle.len())
        .max()
        .unwrap_or_default();
    let mut reader = std::io::BufReader::new(file);
    let mut window = Vec::with_capacity(SCAN_CHUNK_BYTES + longest);
    let mut chunk = vec![0_u8; SCAN_CHUNK_BYTES];
    loop {
        let read = reader
            .read(&mut chunk)
            .map_err(|error| CcBypassReason::InputRead {
                path: path.to_path_buf(),
                message: error.to_string(),
            })?;
        if read == 0 {
            return Ok(false);
        }
        window.extend_from_slice(&chunk[..read]);
        if needles
            .iter()
            .any(|needle| contains_subslice_ascii_case_insensitive(&window, needle))
        {
            return Ok(true);
        }
        let keep = window.len().saturating_sub(longest.saturating_sub(1));
        window.drain(..keep);
    }
}

fn contains_subslice_ascii_case_insensitive(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || haystack.len() < needle.len() {
        return false;
    }
    haystack.windows(needle.len()).any(|window| {
        window
            .iter()
            .zip(needle)
            .all(|(left, right)| left.eq_ignore_ascii_case(right))
    })
}

#[cfg(test)]
#[path = "depfile_tests.rs"]
mod tests;
