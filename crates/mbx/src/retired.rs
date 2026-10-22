//! Trees detached from their live names before slow deletion.

use std::ffi::OsStr;
use std::io;
use std::path::{Path, PathBuf};

pub(crate) struct RetiredTree(PathBuf);

impl RetiredTree {
    /// Call while holding the lock that owns the live name. Rename only
    /// after the final probe; finish bookkeeping before releasing that lock.
    pub(crate) fn retire(live: &Path) -> io::Result<Option<Self>> {
        let Some(name) = live.file_name() else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "tree has no name",
            ));
        };
        let mut retired = name.to_os_string();
        retired.push(format!(
            ".retired-{}-{}",
            std::process::id(),
            crate::util::random_string(24)
        ));
        // macOS requires a writable directory for rename. Do not change
        // file modes: incremental files can share inodes with other trees.
        #[cfg(unix)]
        if let Ok(metadata) = std::fs::symlink_metadata(live)
            && metadata.is_dir()
        {
            use std::os::unix::fs::PermissionsExt as _;
            let permissions = metadata.permissions();
            std::fs::set_permissions(
                live,
                std::fs::Permissions::from_mode(permissions.mode() | 0o700),
            )?;
            if let Err(error) = std::fs::rename(live, live.with_file_name(&retired)) {
                let _ = std::fs::set_permissions(live, permissions);
                return if error.kind() == io::ErrorKind::NotFound {
                    Ok(None)
                } else {
                    Err(error)
                };
            }
            return Ok(Some(Self(live.with_file_name(retired))));
        }
        match std::fs::rename(live, live.with_file_name(&retired)) {
            Ok(()) => Ok(Some(Self(live.with_file_name(retired)))),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

pub(crate) fn is_retired_name(name: &OsStr) -> bool {
    name.to_str().is_some_and(|name| {
        name.rsplit_once(".retired-").is_some_and(|(live, suffix)| {
            !live.is_empty()
                && suffix.split_once('-').is_some_and(|(pid, random)| {
                    !pid.is_empty()
                        && pid.bytes().all(|byte| byte.is_ascii_digit())
                        && !random.is_empty()
                        && random.bytes().all(|byte| byte.is_ascii_alphanumeric())
                })
        })
    })
}
