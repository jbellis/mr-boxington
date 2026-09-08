use crate::CacheDigest;
use serde::{Deserialize, Serialize};
use std::io::{self, Read as _};
use std::path::{Path, PathBuf};
#[cfg(target_os = "linux")]
use std::sync::{Mutex, OnceLock};
use std::time::SystemTime;

const DIGEST_BUFFER_BYTES: usize = 64 * 1024;
const TIMESTAMP_MACROS: &[&[u8]] = &[b"__DATE__", b"__TIME__", b"__TIMESTAMP__"];

#[cfg(target_os = "linux")]
const STATX_IDENTITY_MASK: u32 = 0x100 | 0x200 | 0x40 | 0x1000;

/// Stable Linux UAPI layout. `libc` omits statx on older musl headers even
/// though the kernel syscall and wire structure are available there.
#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxStatxTimestamp {
    seconds: i64,
    nanos: u32,
    reserved: i32,
}

#[cfg(target_os = "linux")]
#[repr(C)]
struct LinuxStatx {
    mask: u32,
    block_size: u32,
    attributes: u64,
    links: u32,
    uid: u32,
    gid: u32,
    mode: u16,
    reserved0: u16,
    inode: u64,
    size: u64,
    blocks: u64,
    attributes_mask: u64,
    accessed: LinuxStatxTimestamp,
    created: LinuxStatxTimestamp,
    changed: LinuxStatxTimestamp,
    modified: LinuxStatxTimestamp,
    rdev_major: u32,
    rdev_minor: u32,
    device_major: u32,
    device_minor: u32,
    mount_id: u64,
    direct_io_memory_alignment: u32,
    direct_io_offset_alignment: u32,
    subvolume: u64,
    atomic_write_unit_min: u32,
    atomic_write_unit_max: u32,
    atomic_write_segments_max: u32,
    direct_io_read_offset_alignment: u32,
    atomic_write_unit_max_opt: u32,
    reserved1: u32,
    reserved2: [u64; 8],
}

#[cfg(target_os = "linux")]
const _: () = assert!(std::mem::size_of::<LinuxStatx>() == 256);

/// The on-disk identity a recorded file digest describes.
///
/// The same trade [`VerifiedBlob`] makes for CAS reads, offered to shims for
/// the files they hash: an overwrite moves the modification time and a
/// truncation changes the length, so a digest recorded against both stands
/// until either does. Where the platform reports a metadata-change time the
/// identity carries that too, and it is the part a writer cannot restore: a
/// rewrite that puts the modification time back still moves the change time,
/// so only filesystems without one fall back to the freshness model the
/// surrounding build tool already lives on.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileIdentity {
    /// Absolute path of the file.
    pub path: PathBuf,
    /// Length of the file in bytes.
    pub len: u64,
    /// Modification time of the file.
    pub modified: SystemTime,
    /// Platform metadata-change token, where one exists.
    pub changed: Option<(i64, i64)>,
    /// Stable object identity where the platform exposes one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub object: Option<FileObjectIdentity>,
}

/// The kernel identity of one file object on one mounted filesystem.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileObjectIdentity {
    /// Device identifier's major or high-order component.
    pub device_major: u32,
    /// Device identifier's minor or low-order component.
    pub device_minor: u32,
    /// Mount identifier in this mount namespace, or zero when unavailable.
    pub mount_id: u64,
    /// Inode number within the mounted filesystem.
    pub inode: u64,
}

impl FileIdentity {
    /// Describe a file from metadata already in hand, or nothing when the
    /// filesystem reports no modification time to compare against later.
    pub fn describe(path: &Path, metadata: &std::fs::Metadata) -> Option<Self> {
        Some(Self {
            path: path.to_path_buf(),
            len: metadata.len(),
            modified: metadata.modified().ok()?,
            changed: change_token(metadata),
            object: metadata_object_identity(metadata),
        })
    }

    /// Describe a file only when metadata can safely stand in for its digest.
    ///
    /// Linux NFS may revise cached change times without a write, so it uses a
    /// cached object/length/mtime identity that deliberately omits ctime. This
    /// is the same timestamp-freshness contract as Cargo and [`VerifiedBlob`];
    /// forcing a server round trip per input would serialize large NFS builds.
    /// If the kernel cannot supply every required field, callers hash instead.
    pub fn for_digest_cache(path: &Path, metadata: &std::fs::Metadata) -> io::Result<Option<Self>> {
        digest_cache_identity(
            path,
            metadata,
            metadata_identity_is_unreliable(path, metadata)?,
        )
    }

    /// Whether the file at this identity's path still has exactly this
    /// identity, so the digest recorded against it still describes the bytes on
    /// disk without reading them again.
    ///
    /// Length alone would miss an overwrite that keeps the size, and the
    /// modification time can be put back by whoever rewrote the file. The
    /// change time cannot be set from user space, so where the platform reports
    /// one a rewrite that restores the old modification time still shows. A
    /// file that has vanished is an error rather than a change, so the caller
    /// can tell the two apart.
    pub fn still_describes(&self) -> std::io::Result<bool> {
        let metadata = std::fs::metadata(&self.path)?;
        Ok(Self::for_digest_cache(&self.path, &metadata)?.as_ref() == Some(self))
    }

    /// Whether this identity is strong enough to stand in for a second read.
    pub fn can_skip_content_verification(&self) -> bool {
        self.changed.is_some() || self.object.is_some()
    }
}

/// A content observation established for one file object.
///
/// An observation is the unit used when a caller needs to compare a file
/// before and after an operation. The digest is never returned (or recorded in
/// a [`FileDigestCache`]) until the read that produced it has been associated
/// with the identity of the file that was read. A changing file therefore
/// yields [`io::ErrorKind::WouldBlock`] instead of a potentially mismatched
/// identity/digest pair. As with ordinary build-tool freshness checks, this
/// assumes inputs are not adversarially changed and restored, including their
/// observable timestamps, entirely between observations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileObservation {
    /// Identity of the file object observed while reading its contents.
    pub identity: FileIdentity,
    /// Content digest read from that file object.
    pub digest: CacheDigest,
}

/// How a later input observation compares with an earlier one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileObservationMatch {
    /// The bytes and file object are compatible with reuse.
    Reusable,
    /// The later digest proves that the input bytes differ.
    Changed,
    /// The bytes match, but the available identity cannot establish that the
    /// same file object was observed (for example, a local replacement where
    /// no object identity is available).
    Indeterminate,
}

/// Result of observing an input under a digest scope.
///
/// C/C++ input scanning has one additional result: a timestamp preprocessor
/// macro makes the digest unsuitable for action reuse. Keeping that result in
/// the shared observation layer lets both compiler adapters use the same
/// identity/read validation without duplicating the scan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileObservationResolution {
    /// A validated file observation.
    Observation(FileObservation),
    /// A C/C++ input contains a time-dependent preprocessor macro.
    EmbeddedTimestampMacro,
}

impl FileObservation {
    /// Capture and hash `path` in one attempt without consulting a digest
    /// cache.
    pub fn capture(path: &Path) -> io::Result<Option<Self>> {
        Self::capture_with_cache(path, &NoFileDigestCache)
    }

    /// Read `path` and retain the bytes while establishing their observation.
    ///
    /// Output publication uses this when it must inspect and hash the same
    /// bytes. Returning both avoids a second full-file read while preserving
    /// the identity checks used by ordinary observations.
    pub fn capture_with_contents(path: &Path) -> io::Result<Option<(Self, Vec<u8>)>> {
        let metadata = std::fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("observation path is not a regular file: {}", path.display()),
            ));
        }
        let unreliable = metadata_identity_is_unreliable(path, &metadata)?;
        let file = std::fs::File::open(path)?;
        let before = file.metadata()?;
        let mut contents = Vec::new();
        (&file).read_to_end(&mut contents)?;
        let after = file.metadata()?;
        let digest = CacheDigest::blake3(&contents);
        let Some(current) = current_file_identity(path, unreliable)? else {
            return Ok(None);
        };
        if !handle_observation_is_stable(path, &before, &after, &current, unreliable, digest.size)?
        {
            return Err(unstable_capture_error());
        }
        Ok(Some((
            Self {
                identity: current,
                digest,
            },
            contents,
        )))
    }

    /// Capture `path`, resolving one cached digest when available.
    ///
    /// A cached digest is accepted only when the path still has the exact
    /// identity used for the lookup. On a cache miss, the file is opened once,
    /// hashed through that handle, and checked against the path after the read
    /// completes. There are deliberately no retries: `WouldBlock` is an
    /// indeterminate cache-validation result and callers should continue with
    /// their ordinary compiler path.
    pub fn capture_with_cache(
        path: &Path,
        digests: &dyn FileDigestCache,
    ) -> io::Result<Option<Self>> {
        let mut observations = Self::capture_many(std::iter::once(path), digests)?;
        Ok(observations.pop().flatten())
    }

    /// Capture several paths while resolving all available cached digests in
    /// one batch. Results retain input order. A single unstable path returns
    /// [`io::ErrorKind::WouldBlock`] for the batch, allowing the caller to
    /// bypass cache publication without accepting any partial observations.
    pub fn capture_many<'a, I, P>(
        paths: I,
        digests: &dyn FileDigestCache,
    ) -> io::Result<Vec<Option<Self>>>
    where
        I: IntoIterator<Item = &'a P>,
        P: AsRef<Path> + ?Sized + 'a,
    {
        Self::capture_many_with_scope(FileDigestScope::Content, paths, digests).map(
            |observations| {
                observations
                    .into_iter()
                    .map(|observation| match observation {
                        Some(FileObservationResolution::Observation(observation)) => {
                            Some(observation)
                        }
                        Some(FileObservationResolution::EmbeddedTimestampMacro) => {
                            unreachable!("content observation cannot contain timestamp macros")
                        }
                        None => None,
                    })
                    .collect()
            },
        )
    }

    /// Capture one path under `scope`, preserving scope-specific outcomes such
    /// as [`FileObservationResolution::EmbeddedTimestampMacro`].
    pub fn capture_with_scope(
        path: &Path,
        scope: FileDigestScope,
        digests: &dyn FileDigestCache,
    ) -> io::Result<Option<FileObservationResolution>> {
        let mut observations =
            Self::capture_many_with_scope(scope, std::iter::once(path), digests)?;
        Ok(observations.pop().flatten())
    }

    /// Capture several paths under `scope` while resolving all available
    /// cached digests in one batch. Results retain input order. A single
    /// unstable path returns [`io::ErrorKind::WouldBlock`] for the batch,
    /// allowing the caller to bypass cache publication without accepting any
    /// partial observations.
    pub fn capture_many_with_scope<'a, I, P>(
        scope: FileDigestScope,
        paths: I,
        digests: &dyn FileDigestCache,
    ) -> io::Result<Vec<Option<FileObservationResolution>>>
    where
        I: IntoIterator<Item = &'a P>,
        P: AsRef<Path> + ?Sized + 'a,
    {
        let mut inputs = Vec::new();
        for path in paths {
            let path = path.as_ref();
            let metadata = std::fs::metadata(path)?;
            if !metadata.is_file() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("observation path is not a regular file: {}", path.display()),
                ));
            }
            let unreliable = metadata_identity_is_unreliable(path, &metadata)?;
            let cache_identity = digest_cache_identity(path, &metadata, unreliable)?;
            let identity = cache_identity
                .clone()
                .or_else(|| FileIdentity::describe(path, &metadata));
            inputs.push(CaptureInput {
                path: path.to_path_buf(),
                unreliable,
                cache_identity,
                identity,
            });
        }

        let queries = inputs
            .iter()
            .filter_map(|input| input.cache_identity.clone())
            .collect::<Vec<_>>();
        let mut resolutions = digests.resolve(scope, &queries).into_iter();
        let mut observations = Vec::with_capacity(inputs.len());
        let mut fresh = Vec::new();
        for input in inputs {
            let Some(identity) = input.identity else {
                observations.push(None);
                continue;
            };
            let resolution = input
                .cache_identity
                .as_ref()
                .and_then(|_| resolutions.next())
                .unwrap_or(FileDigestResolution::Unresolved);
            match resolution {
                FileDigestResolution::Digest(digest) => {
                    if digest.size != identity.len {
                        return Err(unstable_capture_error());
                    }
                    let current = current_file_identity(&input.path, input.unreliable)?;
                    if current.as_ref() != Some(&identity) {
                        return Err(unstable_capture_error());
                    }
                    observations.push(Some(FileObservationResolution::Observation(Self {
                        identity,
                        digest,
                    })));
                    continue;
                }
                FileDigestResolution::EmbeddedTimestampMacro
                    if scope == FileDigestScope::CcInput =>
                {
                    // A cached macro result carries no content digest, but it
                    // is still valid only for the identity used to resolve
                    // it. Do not rescan the file and lose batch coalescing.
                    let current = current_file_identity(&input.path, input.unreliable)?;
                    if current.as_ref() != Some(&identity) {
                        return Err(unstable_capture_error());
                    }
                    observations.push(Some(FileObservationResolution::EmbeddedTimestampMacro));
                    continue;
                }
                FileDigestResolution::EmbeddedTimestampMacro => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "content digest resolver returned a cc-only outcome",
                    ));
                }
                FileDigestResolution::Indeterminate => return Err(unstable_capture_error()),
                FileDigestResolution::Unresolved => {}
            }

            let (resolution, digest_size, before, after) = digest_open_file(scope, &input.path)?;
            let current = current_file_identity(&input.path, input.unreliable)?;
            let Some(current) = current else {
                observations.push(None);
                continue;
            };
            if !handle_observation_is_stable(
                &input.path,
                &before,
                &after,
                &current,
                input.unreliable,
                digest_size,
            )? {
                return Err(unstable_capture_error());
            }
            if resolution == FileDigestResolution::EmbeddedTimestampMacro {
                observations.push(Some(FileObservationResolution::EmbeddedTimestampMacro));
                continue;
            }
            let digest = resolution
                .into_digest()
                .ok_or_else(|| io::Error::other("content digest resolution returned no digest"))?;
            // The digest has now been checked against both the handle and the
            // pathname. Recording it under `current` is important on NFS,
            // where mtime may reconcile while a read is in progress: the
            // bytes belong to the object, length, and final identity observed.
            // Defer the batch record until every input has validated so a
            // later unstable input cannot leave a partial ledger update.
            fresh.push(RecordedFileDigest {
                file: current.clone(),
                digest: digest.clone(),
            });
            observations.push(Some(FileObservationResolution::Observation(Self {
                identity: current,
                digest,
            })));
        }
        if !fresh.is_empty() {
            digests.record(scope, fresh);
        }
        Ok(observations)
    }

    /// Classify a later identity and digest against this observation.
    ///
    /// Timestamps are intentionally excluded from this comparison. The digest
    /// establishes the bytes, while the path, length, and object
    /// identity ensure that the comparison did not silently cross a replaced
    /// file. Cache lookup still uses the complete platform-specific identity.
    /// Matching endpoints cannot prove that a writer briefly changed and then
    /// restored the same object while the compiler ran; normal builds are
    /// assumed not to perform that adversarial sequence.
    pub fn compare(
        &self,
        identity: Option<&FileIdentity>,
        digest: &CacheDigest,
    ) -> FileObservationMatch {
        let Some(identity) = identity else {
            return if self.digest == *digest {
                FileObservationMatch::Indeterminate
            } else {
                FileObservationMatch::Changed
            };
        };
        if self.digest != *digest {
            return FileObservationMatch::Changed;
        }
        if self.digest.size != self.identity.len || digest.size != identity.len {
            return FileObservationMatch::Indeterminate;
        }
        if self.identity == *identity
            || (self.identity.path == identity.path
                && self.identity.len == identity.len
                && self.identity.object.is_some()
                && self.identity.object == identity.object)
        {
            FileObservationMatch::Reusable
        } else {
            FileObservationMatch::Indeterminate
        }
    }

    /// Whether `identity` and `digest` describe a reusable version of this
    /// observation.
    pub fn matches(&self, identity: Option<&FileIdentity>, digest: &CacheDigest) -> bool {
        self.compare(identity, digest) == FileObservationMatch::Reusable
    }
}

struct CaptureInput {
    path: PathBuf,
    unreliable: bool,
    cache_identity: Option<FileIdentity>,
    identity: Option<FileIdentity>,
}

fn unstable_capture_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::WouldBlock,
        "file changed while capturing its content observation",
    )
}

fn current_file_identity(path: &Path, unreliable: bool) -> io::Result<Option<FileIdentity>> {
    let metadata = std::fs::metadata(path)?;
    current_file_identity_from_metadata(path, &metadata, unreliable)
}

fn current_file_identity_from_metadata(
    path: &Path,
    metadata: &std::fs::Metadata,
    unreliable: bool,
) -> io::Result<Option<FileIdentity>> {
    digest_cache_identity(path, metadata, unreliable)
        .map(|identity| identity.or_else(|| FileIdentity::describe(path, metadata)))
}

fn digest_open_file(
    scope: FileDigestScope,
    path: &Path,
) -> io::Result<(
    FileDigestResolution,
    u64,
    std::fs::Metadata,
    std::fs::Metadata,
)> {
    let file = std::fs::File::open(path)?;
    let before = file.metadata()?;
    let (resolution, size) = digest_reader(scope, &file)?;
    let after = file.metadata()?;
    Ok((resolution, size, before, after))
}

/// Resolve one file only when the open handle, its final pathname, and the
/// identity supplied by the caller still name the same stable contents.
pub(super) fn digest_file_for_identity(
    scope: FileDigestScope,
    path: &Path,
    expected: &FileIdentity,
) -> io::Result<FileDigestResolution> {
    let (resolution, size, before, after) = digest_open_file(scope, path)?;
    // The expected identity was already qualified at the caller. On Linux,
    // the qualified NFS form is the one with an object identity and no ctime;
    // derive policy from it so a concurrent cross-mount rename cannot poison
    // the filesystem-policy memo while this read is being rejected.
    #[cfg(target_os = "linux")]
    let unreliable = expected.changed.is_none() && expected.object.is_some();
    #[cfg(not(target_os = "linux"))]
    let unreliable = false;
    let Some(current) = current_file_identity(path, unreliable)? else {
        return Err(unstable_capture_error());
    };
    if current != *expected
        || !handle_observation_is_stable(path, &before, &after, &current, unreliable, size)?
    {
        return Err(unstable_capture_error());
    }
    Ok(resolution)
}

fn digest_reader(
    scope: FileDigestScope,
    file: &std::fs::File,
) -> io::Result<(FileDigestResolution, u64)> {
    let mut reader = std::io::BufReader::new(file);
    let mut hasher = blake3::Hasher::new();
    let mut size = 0_u64;
    let longest_macro = TIMESTAMP_MACROS
        .iter()
        .map(|macro_name| macro_name.len())
        .max()
        .unwrap_or_default();
    let mut window = Vec::with_capacity(DIGEST_BUFFER_BYTES + longest_macro);
    let mut chunk = vec![0_u8; DIGEST_BUFFER_BYTES];
    let mut found_timestamp_macro = false;
    loop {
        let read = reader.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        hasher.update(&chunk[..read]);
        size = size
            .checked_add(read as u64)
            .ok_or_else(|| io::Error::other("file length overflowed u64"))?;
        if scope == FileDigestScope::CcInput && !found_timestamp_macro {
            window.extend_from_slice(&chunk[..read]);
            found_timestamp_macro = TIMESTAMP_MACROS
                .iter()
                .any(|macro_name| contains_subslice(&window, macro_name));
            let keep = window.len().saturating_sub(longest_macro.saturating_sub(1));
            window.drain(..keep);
        }
    }
    if found_timestamp_macro {
        Ok((FileDigestResolution::EmbeddedTimestampMacro, size))
    } else {
        Ok((
            FileDigestResolution::Digest(CacheDigest {
                algorithm: "blake3".into(),
                hash: hasher.finalize().to_hex().to_string(),
                size,
            }),
            size,
        ))
    }
}

fn handle_observation_is_stable(
    path: &Path,
    before: &std::fs::Metadata,
    after: &std::fs::Metadata,
    current: &FileIdentity,
    unreliable: bool,
    digest_size: u64,
) -> io::Result<bool> {
    if before.len() != after.len() || after.len() != digest_size || current.len != digest_size {
        return Ok(false);
    }
    if unreliable {
        #[cfg(target_os = "linux")]
        {
            let before_modified = before.modified()?;
            let after_modified = after.modified()?;
            return Ok(current.object.as_ref().is_some_and(|object| {
                before_modified == after_modified
                    && after_modified == current.modified
                    && metadata_matches_object(before, object)
                    && metadata_matches_object(after, object)
            }));
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (path, before, after, current);
            return Ok(false);
        }
    }
    let before = FileIdentity::describe(path, before);
    let after = FileIdentity::describe(path, after);
    Ok(before.is_some() && before == after && after.as_ref() == Some(current))
}

#[cfg(target_os = "linux")]
fn metadata_object_identity(metadata: &std::fs::Metadata) -> Option<FileObjectIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let device = metadata.dev();
    let major = ((device >> 8) & 0x0fff) | ((device >> 32) & 0xfffff000);
    let minor = (device & 0x00ff) | ((device >> 12) & 0xffffff00);
    Some(FileObjectIdentity {
        device_major: major.try_into().ok()?,
        device_minor: minor.try_into().ok()?,
        mount_id: 0,
        inode: metadata.ino(),
    })
}

#[cfg(all(unix, not(target_os = "linux")))]
fn metadata_object_identity(metadata: &std::fs::Metadata) -> Option<FileObjectIdentity> {
    use std::os::unix::fs::MetadataExt as _;

    let device = metadata.dev();
    Some(FileObjectIdentity {
        device_major: (device >> 32) as u32,
        device_minor: device as u32,
        mount_id: 0,
        inode: metadata.ino(),
    })
}

#[cfg(not(unix))]
fn metadata_object_identity(_metadata: &std::fs::Metadata) -> Option<FileObjectIdentity> {
    None
}

/// Compare the device and inode in `metadata` with a `statx` object identity.
/// `statx` also supplies a mount id, which is intentionally left to the path
/// check: an fd has no portable mount-id query, while the final path identity
/// still catches a replacement on another mount.
#[cfg(target_os = "linux")]
fn metadata_matches_object(metadata: &std::fs::Metadata, object: &FileObjectIdentity) -> bool {
    metadata_object_identity(metadata).is_some_and(|metadata| {
        metadata.device_major == object.device_major
            && metadata.device_minor == object.device_minor
            && metadata.inode == object.inode
    })
}

/// Legacy snapshot wrapper retained for adapters that have not migrated to
/// [`FileObservation`]. New code should carry the observation directly; this
/// wrapper no longer has a separate metadata-versus-content capture path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileSnapshot {
    identity: FileIdentity,
    observation: Option<FileObservation>,
}

impl FileSnapshot {
    /// Capture a validated content observation for compatibility callers.
    pub fn capture(path: &Path) -> io::Result<Option<Self>> {
        FileObservation::capture(path).map(|observation| {
            observation.map(|observation| Self {
                identity: observation.identity.clone(),
                observation: Some(observation),
            })
        })
    }

    /// Capture a validated content observation using the session digest cache.
    pub fn capture_with_cache(
        path: &Path,
        digests: &dyn FileDigestCache,
    ) -> io::Result<Option<Self>> {
        FileObservation::capture_with_cache(path, digests).map(|observation| {
            observation.map(|observation| Self {
                identity: observation.identity.clone(),
                observation: Some(observation),
            })
        })
    }

    /// Whether a later identity and digest still match this compatibility
    /// snapshot.
    pub fn matches(&self, identity: Option<&FileIdentity>, content: &CacheDigest) -> bool {
        self.observation.as_ref().map_or_else(
            || identity == Some(&self.identity),
            |observation| observation.matches(identity, content),
        )
    }

    /// Whether this snapshot carries content evidence for a later comparison.
    pub fn proves_content_change(&self) -> bool {
        self.observation.is_some() || self.identity.changed.is_some()
    }
}

impl From<FileIdentity> for FileSnapshot {
    fn from(identity: FileIdentity) -> Self {
        Self {
            identity,
            observation: None,
        }
    }
}

#[cfg(target_os = "linux")]
fn metadata_identity_is_unreliable(path: &Path, metadata: &std::fs::Metadata) -> io::Result<bool> {
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::MetadataExt as _;

    static FILESYSTEMS: OnceLock<Mutex<std::collections::BTreeMap<u64, bool>>> = OnceLock::new();
    let filesystems = FILESYSTEMS.get_or_init(|| Mutex::new(std::collections::BTreeMap::new()));
    if let Some(unreliable) = filesystems.lock().unwrap().get(&metadata.dev()).copied() {
        return Ok(unreliable);
    }

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut status = MaybeUninit::<libc::statfs>::zeroed();
    // SAFETY: `path` is NUL-terminated and `status` points to writable,
    // correctly sized storage. statfs initializes it before returning success.
    let result = unsafe { libc::statfs(path.as_ptr(), status.as_mut_ptr()) };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: statfs returned success and initialized `status`.
    let status = unsafe { status.assume_init() };
    // libc gives NFS_SUPER_MAGIC a different signedness from statfs::f_type on musl.
    let unreliable = status.f_type == 0x6969;
    filesystems
        .lock()
        .unwrap()
        .insert(metadata.dev(), unreliable);
    Ok(unreliable)
}

#[cfg(not(target_os = "linux"))]
fn metadata_identity_is_unreliable(
    _path: &Path,
    _metadata: &std::fs::Metadata,
) -> io::Result<bool> {
    Ok(false)
}

fn digest_cache_identity(
    path: &Path,
    metadata: &std::fs::Metadata,
    unreliable: bool,
) -> io::Result<Option<FileIdentity>> {
    if unreliable {
        nfs_file_identity(path)
    } else {
        Ok(FileIdentity::describe(path, metadata))
    }
}

#[cfg(target_os = "linux")]
fn nfs_file_identity(path: &Path) -> io::Result<Option<FileIdentity>> {
    use std::mem::MaybeUninit;
    use std::os::unix::ffi::OsStrExt as _;

    let path = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
    let mut status = MaybeUninit::<LinuxStatx>::zeroed();
    // SAFETY: `path` is NUL-terminated and `status` names writable storage.
    let result = unsafe {
        libc::syscall(
            libc::SYS_statx,
            libc::AT_FDCWD,
            path.as_ptr(),
            // AT_STATX_DONT_SYNC: the surrounding build already trusts cached
            // mtimes, and FORCE_SYNC costs one NFS RPC for every dependency
            // edge rather than every distinct file.
            0x4000,
            STATX_IDENTITY_MASK,
            status.as_mut_ptr(),
        )
    };
    if result != 0 {
        let error = io::Error::last_os_error();
        return match error.raw_os_error() {
            Some(libc::ENOSYS | libc::EINVAL | libc::EOPNOTSUPP) => Ok(None),
            _ => Err(error),
        };
    }
    // SAFETY: statx returned success and initialized `status`.
    let status = unsafe { status.assume_init() };
    nfs_identity_from_statx(
        Path::new(std::ffi::OsStr::from_bytes(path.to_bytes())),
        &status,
    )
}

#[cfg(target_os = "linux")]
fn nfs_identity_from_statx(path: &Path, status: &LinuxStatx) -> io::Result<Option<FileIdentity>> {
    if status.mask & STATX_IDENTITY_MASK != STATX_IDENTITY_MASK
        || status.modified.nanos >= 1_000_000_000
    {
        return Ok(None);
    }
    let modified = system_time(status.modified.seconds, status.modified.nanos)?;
    Ok(Some(FileIdentity {
        path: path.to_path_buf(),
        len: status.size,
        modified,
        changed: None,
        object: Some(FileObjectIdentity {
            device_major: status.device_major,
            device_minor: status.device_minor,
            mount_id: status.mount_id,
            inode: status.inode,
        }),
    }))
}

#[cfg(not(target_os = "linux"))]
fn nfs_file_identity(_path: &Path) -> io::Result<Option<FileIdentity>> {
    Ok(None)
}

#[cfg(target_os = "linux")]
fn system_time(seconds: i64, nanos: u32) -> io::Result<SystemTime> {
    if seconds >= 0 {
        SystemTime::UNIX_EPOCH.checked_add(std::time::Duration::new(seconds as u64, nanos))
    } else {
        SystemTime::UNIX_EPOCH
            .checked_sub(std::time::Duration::from_secs(seconds.unsigned_abs()))
            .and_then(|time| time.checked_add(std::time::Duration::from_nanos(nanos.into())))
    }
    .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "file timestamp is out of range"))
}

/// The metadata-change time as an opaque token, where the platform has one.
#[cfg(unix)]
fn change_token(metadata: &std::fs::Metadata) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.ctime(), metadata.ctime_nsec()))
}

/// Windows reports creation rather than metadata-change time, which a rewrite
/// does not move, so no token is better than a misleading one.
#[cfg(not(unix))]
fn change_token(_metadata: &std::fs::Metadata) -> Option<(i64, i64)> {
    None
}

/// A file digest recorded against the identity it was read under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordedFileDigest {
    /// Identity of the file when its contents were hashed.
    pub file: FileIdentity,
    /// Digest of those contents.
    pub digest: CacheDigest,
}

/// What a recorded file digest may stand in for.
///
/// Adapters prove different things when they read a file: the cc adapter's
/// input scan also establishes that a source embeds no timestamp macro, which
/// a digest recorded by the rustc adapter never checked. Scoping the ledger
/// keeps one adapter's shortcut from resting on a property another adapter
/// never established.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileDigestScope {
    /// The digest describes the file's contents and nothing more.
    Content,
    /// The digest describes a cc compiler input that also passed the
    /// timestamp-macro scan.
    CcInput,
}

/// The outcome of resolving one file under a digest scope.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum FileDigestResolution {
    /// The file's content digest, with any scope-specific checks complete.
    Digest(CacheDigest),
    /// A C/C++ input contains a time-dependent preprocessor macro.
    EmbeddedTimestampMacro,
    /// A resolver attempted to observe the file, but could not associate the
    /// result with a stable identity. Callers must bypass rather than retry.
    Indeterminate,
    /// No shared resolver was available; the caller must read the file.
    Unresolved,
}

impl FileDigestResolution {
    /// Extract the digest when resolution succeeded.
    pub fn into_digest(self) -> Option<CacheDigest> {
        match self {
            Self::Digest(digest) => Some(digest),
            Self::EmbeddedTimestampMacro | Self::Indeterminate | Self::Unresolved => None,
        }
    }
}

/// Read and hash a file once, applying the checks required by `scope` in that
/// same pass.
pub fn digest_file(scope: FileDigestScope, path: &Path) -> io::Result<FileDigestResolution> {
    let file = std::fs::File::open(path)?;
    digest_reader(scope, &file).map(|(resolution, _)| resolution)
}

fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty()
        && haystack.len() >= needle.len()
        && haystack
            .windows(needle.len())
            .any(|window| window == needle)
}

/// Recorded digests a session may consult instead of rehashing a file.
///
/// The agent's file-digest ledger answers through this everywhere a caller
/// holds one; the no-op implementation stands in where reuse must not happen,
/// such as under verification.
pub trait FileDigestCache: Send + Sync {
    /// Resolve these identities, coalescing concurrent misses where supported.
    fn resolve(&self, scope: FileDigestScope, files: &[FileIdentity]) -> Vec<FileDigestResolution> {
        self.find(scope, files)
            .into_iter()
            .map(|digest| {
                digest.map_or(
                    FileDigestResolution::Unresolved,
                    FileDigestResolution::Digest,
                )
            })
            .collect()
    }
    /// Recorded digests for these identities, in request order.
    fn find(&self, scope: FileDigestScope, files: &[FileIdentity]) -> Vec<Option<CacheDigest>>;
    /// Record digests for files read under these identities.
    fn record(&self, scope: FileDigestScope, entries: Vec<RecordedFileDigest>);
}

/// A [`FileDigestCache`] that remembers nothing and finds nothing.
pub struct NoFileDigestCache;

impl FileDigestCache for NoFileDigestCache {
    fn resolve(
        &self,
        _scope: FileDigestScope,
        files: &[FileIdentity],
    ) -> Vec<FileDigestResolution> {
        vec![FileDigestResolution::Unresolved; files.len()]
    }

    fn find(&self, _scope: FileDigestScope, files: &[FileIdentity]) -> Vec<Option<CacheDigest>> {
        vec![None; files.len()]
    }

    fn record(&self, _scope: FileDigestScope, _entries: Vec<RecordedFileDigest>) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    struct ReplacingDigestCache {
        replacements: std::sync::atomic::AtomicUsize,
        contents: &'static [u8],
        timestamp_only: bool,
        recorded: Mutex<Vec<RecordedFileDigest>>,
    }

    #[cfg(target_os = "linux")]
    impl FileDigestCache for ReplacingDigestCache {
        fn find(&self, _: FileDigestScope, files: &[FileIdentity]) -> Vec<Option<CacheDigest>> {
            if self
                .replacements
                .fetch_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |remaining| remaining.checked_sub(1),
                )
                .is_ok()
            {
                // A writer finishes before resolution returns, while no
                // compiler has started. The old inode is still live when the
                // replacement is created, making the identity change exact.
                let path = &files[0].path;
                if self.timestamp_only {
                    std::fs::File::options()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
                        .unwrap();
                } else {
                    let replacement = path.with_extension("replacement");
                    std::fs::write(&replacement, self.contents).unwrap();
                    std::fs::rename(replacement, path).unwrap();
                }
            }
            vec![None; files.len()]
        }

        fn record(&self, _: FileDigestScope, entries: Vec<RecordedFileDigest>) {
            self.recorded.lock().unwrap().extend(entries);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_uses_the_replacement_identity_after_resolution() {
        for contents in [
            b"original bytes".as_slice(),
            b"a different replacement".as_slice(),
        ] {
            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("input.rmeta");
            std::fs::write(&path, b"original bytes").unwrap();
            let cache = ReplacingDigestCache {
                replacements: 1.into(),
                contents,
                timestamp_only: false,
                recorded: Mutex::new(Vec::new()),
            };
            let observation = FileObservation::capture_with_cache(&path, &cache)
                .unwrap()
                .unwrap();
            let current =
                FileIdentity::describe(&path, &std::fs::metadata(&path).unwrap()).unwrap();
            let digest = CacheDigest::blake3_file(&path).unwrap();
            assert!(observation.matches(Some(&current), &digest));
            assert!(
                cache
                    .recorded
                    .lock()
                    .unwrap()
                    .iter()
                    .all(|record| record.file == current && record.digest == digest),
                "a digest must never be published under the replaced file's identity"
            );
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn an_unstable_cached_digest_returns_would_block_without_retrying() {
        struct UpdatingCache {
            calls: std::sync::atomic::AtomicUsize,
            recorded: Mutex<Vec<RecordedFileDigest>>,
        }
        impl FileDigestCache for UpdatingCache {
            fn find(&self, _: FileDigestScope, files: &[FileIdentity]) -> Vec<Option<CacheDigest>> {
                self.calls
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                let path = &files[0].path;
                let digest = CacheDigest::blake3_file(path).unwrap();
                std::fs::write(path, b"after!").unwrap();
                vec![Some(digest)]
            }

            fn record(&self, _: FileDigestScope, entries: Vec<RecordedFileDigest>) {
                self.recorded.lock().unwrap().extend(entries);
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.rmeta");
        std::fs::write(&path, b"before!").unwrap();
        let cache = UpdatingCache {
            calls: 0.into(),
            recorded: Mutex::new(Vec::new()),
        };
        let result = FileObservation::capture_with_cache(&path, &cache);
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
        assert_eq!(cache.calls.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(cache.recorded.lock().unwrap().is_empty());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn observation_accepts_timestamp_reconciliation_before_hashing() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.rmeta");
        std::fs::write(&path, b"original bytes").unwrap();
        let cache = ReplacingDigestCache {
            replacements: 1.into(),
            contents: b"original bytes",
            timestamp_only: true,
            recorded: Mutex::new(Vec::new()),
        };
        let observation = FileObservation::capture_with_cache(&path, &cache)
            .unwrap()
            .unwrap();
        let current = FileIdentity::describe(&path, &std::fs::metadata(&path).unwrap()).unwrap();
        let digest = CacheDigest::blake3_file(&path).unwrap();
        assert!(observation.matches(Some(&current), &digest));
        assert_eq!(
            *cache.recorded.lock().unwrap(),
            vec![RecordedFileDigest {
                file: current,
                digest
            }],
            "only the reconciled cache identity should be published"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn a_cached_digest_changed_during_resolution_is_not_reused() {
        struct UpdatingCache(std::sync::atomic::AtomicBool);
        impl FileDigestCache for UpdatingCache {
            fn find(&self, _: FileDigestScope, files: &[FileIdentity]) -> Vec<Option<CacheDigest>> {
                let path = &files[0].path;
                let digest = CacheDigest::blake3_file(path).unwrap();
                if self.0.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    // The old cache entry answered correctly when queried,
                    // but a same-length write completed before it returned.
                    // Only mtime changes in the NFS cache identity.
                    std::fs::write(path, b"after!").unwrap();
                    std::fs::File::options()
                        .write(true)
                        .open(path)
                        .unwrap()
                        .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
                        .unwrap();
                }
                vec![Some(digest)]
            }

            fn record(&self, _: FileDigestScope, _: Vec<RecordedFileDigest>) {
                panic!("both attempts should reuse cached digests");
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.rmeta");
        std::fs::write(&path, b"before").unwrap();
        let result = FileObservation::capture_with_cache(&path, &UpdatingCache(true.into()));
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn observations_resolve_a_batch_and_record_only_validated_pairs() {
        struct BatchCache {
            resolves: std::sync::atomic::AtomicUsize,
            recorded: Mutex<Vec<RecordedFileDigest>>,
        }
        impl FileDigestCache for BatchCache {
            fn resolve(
                &self,
                _: FileDigestScope,
                files: &[FileIdentity],
            ) -> Vec<FileDigestResolution> {
                self.resolves
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                vec![FileDigestResolution::Unresolved; files.len()]
            }

            fn find(&self, _: FileDigestScope, files: &[FileIdentity]) -> Vec<Option<CacheDigest>> {
                vec![None; files.len()]
            }

            fn record(&self, _: FileDigestScope, entries: Vec<RecordedFileDigest>) {
                self.recorded.lock().unwrap().extend(entries);
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.rlib");
        let second = directory.path().join("second.rlib");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let paths = [first.as_path(), second.as_path()];
        let cache = BatchCache {
            resolves: 0.into(),
            recorded: Mutex::new(Vec::new()),
        };

        let observations = FileObservation::capture_many(paths, &cache).unwrap();
        assert_eq!(observations.len(), 2);
        assert!(observations.iter().all(Option::is_some));
        assert_eq!(cache.resolves.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert_eq!(cache.recorded.lock().unwrap().len(), 2);
    }

    #[test]
    fn a_failed_batch_records_no_partial_observations() {
        struct DeletingCache {
            delete: PathBuf,
            recorded: Mutex<Vec<RecordedFileDigest>>,
        }

        impl FileDigestCache for DeletingCache {
            fn resolve(
                &self,
                _: FileDigestScope,
                files: &[FileIdentity],
            ) -> Vec<FileDigestResolution> {
                std::fs::remove_file(&self.delete).unwrap();
                vec![FileDigestResolution::Unresolved; files.len()]
            }

            fn find(&self, _: FileDigestScope, files: &[FileIdentity]) -> Vec<Option<CacheDigest>> {
                vec![None; files.len()]
            }

            fn record(&self, _: FileDigestScope, entries: Vec<RecordedFileDigest>) {
                self.recorded.lock().unwrap().extend(entries);
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let first = directory.path().join("first.rlib");
        let second = directory.path().join("second.rlib");
        std::fs::write(&first, b"first").unwrap();
        std::fs::write(&second, b"second").unwrap();
        let cache = DeletingCache {
            delete: second.clone(),
            recorded: Mutex::new(Vec::new()),
        };

        FileObservation::capture_many([first.as_path(), second.as_path()], &cache).unwrap_err();
        assert!(cache.recorded.lock().unwrap().is_empty());
    }

    #[test]
    fn an_indeterminate_shared_observation_is_not_retried_locally() {
        struct IndeterminateCache;

        impl FileDigestCache for IndeterminateCache {
            fn resolve(
                &self,
                _: FileDigestScope,
                files: &[FileIdentity],
            ) -> Vec<FileDigestResolution> {
                vec![FileDigestResolution::Indeterminate; files.len()]
            }

            fn find(&self, _: FileDigestScope, _: &[FileIdentity]) -> Vec<Option<CacheDigest>> {
                panic!("capture must use the explicit resolution outcome")
            }

            fn record(&self, _: FileDigestScope, _: Vec<RecordedFileDigest>) {
                panic!("an indeterminate observation must not be recorded")
            }
        }

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.rlib");
        std::fs::write(&path, b"stable bytes").unwrap();

        let error = FileObservation::capture_with_cache(&path, &IndeterminateCache).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[cfg(unix)]
    #[test]
    fn handle_validation_rejects_a_path_replaced_after_open() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.rlib");
        std::fs::write(&path, b"old bytes").unwrap();
        let file = std::fs::File::open(&path).unwrap();
        let before = file.metadata().unwrap();

        let replacement = directory.path().join("replacement.rlib");
        std::fs::write(&replacement, b"new bytes").unwrap();
        std::fs::rename(&replacement, &path).unwrap();

        let mut bytes = Vec::new();
        (&file).read_to_end(&mut bytes).unwrap();
        let after = file.metadata().unwrap();
        let current = current_file_identity(&path, false).unwrap().unwrap();
        assert!(
            !handle_observation_is_stable(
                &path,
                &before,
                &after,
                &current,
                false,
                bytes.len() as u64,
            )
            .unwrap()
        );
    }

    #[test]
    fn scoped_observation_preserves_timestamp_macro_bypass() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.h");
        std::fs::write(&path, b"const char *build = __TIME__;\n").unwrap();

        let result = FileObservation::capture_with_scope(
            &path,
            FileDigestScope::CcInput,
            &NoFileDigestCache,
        )
        .unwrap();
        assert_eq!(
            result,
            Some(FileObservationResolution::EmbeddedTimestampMacro)
        );
    }

    #[test]
    fn observation_comparison_distinguishes_changed_and_indeterminate_files() {
        let identity = FileIdentity {
            path: PathBuf::from("/tmp/input.rlib"),
            len: 4,
            modified: SystemTime::UNIX_EPOCH,
            changed: Some((1, 0)),
            object: None,
        };
        let observation = FileObservation {
            identity: identity.clone(),
            digest: CacheDigest::blake3(b"same"),
        };
        assert_eq!(
            observation.compare(Some(&identity), &observation.digest),
            FileObservationMatch::Reusable
        );

        let mut timestamp_changed = identity.clone();
        timestamp_changed.modified += std::time::Duration::from_secs(1);
        assert_eq!(
            observation.compare(Some(&timestamp_changed), &observation.digest),
            FileObservationMatch::Indeterminate
        );
        assert_eq!(
            observation.compare(Some(&identity), &CacheDigest::blake3(b"diff")),
            FileObservationMatch::Changed
        );
    }

    #[test]
    fn an_identity_describes_the_file_until_it_is_written_or_removed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.rs");
        std::fs::write(&path, b"fn main() {}").unwrap();
        let identity = FileIdentity::describe(&path, &std::fs::metadata(&path).unwrap()).unwrap();
        assert!(identity.still_describes().unwrap());

        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"fn main() { }").unwrap();
        assert!(!identity.still_describes().unwrap());

        std::fs::remove_file(&path).unwrap();
        assert!(identity.still_describes().is_err());
    }

    #[test]
    fn reliable_metadata_keeps_the_native_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("input.rs");
        std::fs::write(&path, b"fn main() {}").unwrap();
        let metadata = std::fs::metadata(&path).unwrap();

        let identity = digest_cache_identity(&path, &metadata, false)
            .unwrap()
            .unwrap();

        assert_eq!(identity.path, path);
        assert_eq!(identity.len, 12);
        assert_eq!(identity.object.is_some(), cfg!(unix));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn incomplete_statx_metadata_falls_back_to_content_hashing() {
        // SAFETY: all-zero is a valid unpopulated statx result for this parser.
        let status = unsafe { std::mem::zeroed::<LinuxStatx>() };
        assert!(
            nfs_identity_from_statx(Path::new("/nfs/input.rlib"), &status)
                .unwrap()
                .is_none()
        );
    }
}
