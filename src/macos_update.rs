use crate::recovery::LockedRecord;
use crate::APP_ID;
use objc2_foundation::{NSBundle, NSURL};
use std::ffi::{CString, OsString};
use std::fmt;
use std::fs::{self, File, Metadata, TryLockError};
use std::io::{self, Write};
use std::os::fd::AsRawFd;
use std::os::unix::{
    ffi::OsStrExt,
    fs::{MetadataExt, PermissionsExt},
};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitStatus};
use std::ptr::NonNull;
use tempfile::{Builder, TempDir};

const APP_NAME: &str = "PawCommander.app";
const BACKUP_PREFIX: &str = ".pawcommander-previous-";
const VERIFY_SCRIPT: &str = include_str!("../scripts/verify-macos-release.sh");
#[cfg(target_arch = "aarch64")]
const RELEASE_ARCH: (&str, u32) = (
    "arm64",
    objc2_foundation::NSBundleExecutableArchitectureARM64,
);
#[cfg(target_arch = "x86_64")]
const RELEASE_ARCH: (&str, u32) = (
    "x86_64",
    objc2_foundation::NSBundleExecutableArchitectureX86_64,
);

#[derive(Debug)]
pub(crate) enum Error {
    Path {
        path: PathBuf,
        source: io::Error,
    },
    Command {
        program: OsString,
        status: ExitStatus,
        diagnostic: String,
    },
    Refused(String),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let message = match self {
            Self::Path { path, source } => crate::tr_format!(
                "Operation failed at {path:?} ({error})",
                path = path,
                error = source
            ),
            Self::Command {
                program,
                status,
                diagnostic,
            } => crate::tr_format!(
                "Command {program:?} failed ({error})",
                program = program,
                error = format_args!("{status} {diagnostic}")
            ),
            Self::Refused(message) => return formatter.write_str(message),
        };
        formatter.write_str(&message)
    }
}

type Result<T> = std::result::Result<T, Error>;

pub(crate) fn install(installed: &Path, archive: &Path, key: &Path) -> Result<()> {
    let target = Target::open(installed)?;
    let installed_version = bundle_property(&target.path, "CFBundleShortVersionString")?;
    let current = parse_version(&installed_version).ok_or_else(|| invalid_bundle(&target.path))?;
    let (release, candidate) = release_version(archive)?;
    if candidate <= current {
        return Err(Error::Refused(crate::tr_format!(
            "Release {release} must be newer than installed version {installed}.",
            release = release,
            installed = installed_version
        )));
    }

    verify_bundle(&target.path)?;
    let staging = temporary_in(target.directory(), ".pawcommander-update-")?;
    let new_app = prepare_release(staging.path(), archive, key, release)?;

    let backup = temporary_in(target.directory(), BACKUP_PREFIX)?;
    let previous_app = backup.path().join(APP_NAME);
    copy_item(&target.path, &previous_app)?;
    verify_bundle(&previous_app)?;
    let previous_app = backup.keep().join(APP_NAME);
    println!(
        "{}",
        crate::tr_format!(
            "Previous installation saved at {path:?}.",
            path = previous_app
        )
    );
    io::stdout().flush().map_err(|source| Error::Path {
        path: previous_app,
        source,
    })?;

    target.replace_with(&new_app)?;
    println!(
        "{}",
        crate::tr_format!(
            "Installed PawCommander {version} at {path:?}.",
            version = release,
            path = target.path
        )
    );
    Ok(())
}

pub(crate) fn restore(installed: &Path, backup: &Path) -> Result<()> {
    let target = Target::open(installed)?;
    let backup = absolute(backup)?;
    let Some(directory) = backup.parent().filter(|directory| {
        directory.file_name().is_some_and(|name| {
            name.as_encoded_bytes()
                .starts_with(BACKUP_PREFIX.as_bytes())
        }) && backup.file_name().is_some_and(|name| name == APP_NAME)
    }) else {
        return Err(invalid_backup(&backup));
    };
    let parent = directory.parent().expect("backup directory has a parent");
    if at(parent, fs::canonicalize(parent))? != target.directory() {
        return Err(invalid_backup(&backup));
    }
    let metadata = at(directory, fs::symlink_metadata(directory))?;
    // SAFETY: geteuid takes no arguments and always returns the process's effective user ID.
    let user = unsafe { libc::geteuid() };
    if !metadata.is_dir() || metadata.uid() != user || metadata.mode() & 0o077 != 0 {
        return Err(invalid_backup(&backup));
    }
    verify_bundle(&backup)?;

    let staging = temporary_in(target.directory(), ".pawcommander-update-")?;
    let restored = staging.path().join(APP_NAME);
    copy_item(&backup, &restored)?;
    verify_bundle(&restored)?;
    target.replace_with(&restored)?;
    println!(
        "{}",
        crate::tr_format!(
            "Restored {path:?} from {backup:?}.",
            path = target.path,
            backup = backup
        )
    );
    Ok(())
}

struct Target {
    path: PathBuf,
    installed: Metadata,
    parent_lock: LockedRecord,
}

impl Target {
    fn open(path: &Path) -> Result<Self> {
        let path = absolute(path)?;
        let Some(name) = path
            .file_name()
            .filter(|_| path.extension().is_some_and(|ext| ext == "app"))
        else {
            return Err(invalid_bundle(&path));
        };
        let parent = path.parent().expect("application path has a parent");
        let parent = at(parent, fs::canonicalize(parent))?;
        let path = parent.join(name);
        let file = at(&parent, File::open(&parent))?;
        let parent_lock = match LockedRecord::try_lock(file) {
            Ok(lock) => lock,
            Err(TryLockError::WouldBlock) => {
                return Err(Error::Refused(crate::tr_format!(
                    "Another update or restore is using {path:?}.",
                    path = parent
                )));
            }
            Err(TryLockError::Error(error)) => return at(&parent, Err(error)),
        };
        let installed = at(&path, fs::symlink_metadata(&path))?;
        check_bundle(&path, &installed)?;
        require_closed(&path)?;
        Ok(Self {
            path,
            installed,
            parent_lock,
        })
    }

    fn directory(&self) -> &Path {
        self.path.parent().expect("application path has a parent")
    }

    fn replace_with(&self, staged: &Path) -> Result<()> {
        let parent = self.directory();
        let locked_parent = self.parent_lock.as_file();
        let original_parent = at(parent, locked_parent.metadata())?;
        let current_parent = at(parent, fs::symlink_metadata(parent))?;
        let current_app = at(&self.path, fs::symlink_metadata(&self.path))?;
        if !same_file(&original_parent, &current_parent)
            || !same_file(&self.installed, &current_app)
        {
            return Err(Error::Refused(crate::tr_format!(
                "The installation at {path:?} changed during the operation.",
                path = self.path
            )));
        }
        require_closed(&self.path)?;

        let stage_parent = staged.parent().expect("staged application has a parent");
        let stage_parent = at(stage_parent, File::open(stage_parent))?;
        let name = self.path.file_name().expect("application path has a name");
        let name = CString::new(name.as_bytes()).expect("an existing path cannot contain a NUL");
        // SAFETY: Both directory handles and C strings are valid for this call. RENAME_SWAP
        // atomically exchanges the two names on supported filesystems.
        let result = unsafe {
            libc::renameatx_np(
                stage_parent.as_raw_fd(),
                c"PawCommander.app".as_ptr(),
                locked_parent.as_raw_fd(),
                name.as_ptr(),
                libc::RENAME_SWAP,
            )
        };
        if result == 0 {
            Ok(())
        } else {
            at(&self.path, Err(io::Error::last_os_error()))
        }
    }
}

fn prepare_release(staging: &Path, archive: &Path, key: &Path, release: &str) -> Result<PathBuf> {
    let archive = absolute(archive)?;
    let downloads = archive.parent().expect("release archive has a parent");
    let copy = staging.join(archive.file_name().expect("release archive has a name"));
    let trusted_key = staging.join("release.pub");
    for (source, destination) in [
        (archive.clone(), copy.clone()),
        (
            downloads.join("SHA256SUMS.txt"),
            staging.join("SHA256SUMS.txt"),
        ),
        (
            downloads.join("SHA256SUMS.txt.sig"),
            staging.join("SHA256SUMS.txt.sig"),
        ),
        (absolute(key)?, trusted_key.clone()),
    ] {
        if !at(&source, fs::symlink_metadata(&source))?.is_file() {
            return Err(Error::Refused(crate::tr_format!(
                "Expected a regular file at {path:?}.",
                path = source
            )));
        }
        copy_item(&source, &destination)?;
    }
    run(Command::new("/bin/bash")
        .arg("-p")
        .args(["-c", VERIFY_SCRIPT, "verify-macos-release"])
        .arg(trusted_key)
        .arg(&copy))?;

    let extracted = staging.join("extracted");
    run(Command::new("/usr/bin/ditto")
        .args(["-x", "-k", "--qtn"])
        .arg(&copy)
        .arg(&extracted))?;
    let app = extracted.join(APP_NAME);
    verify_bundle(&app)?;
    require_architecture(&app)?;
    if bundle_property(&app, "CFBundleShortVersionString")? != release {
        return Err(Error::Refused(crate::tr_format!(
            "The application in the archive must have release version {version}.",
            version = release
        )));
    }
    let minimum = bundle_property(&app, "LSMinimumSystemVersion")?;
    let required = parse_version(&minimum).ok_or_else(|| invalid_bundle(&app))?;
    let system = run(Command::new("/usr/bin/sw_vers").arg("-productVersion"))?;
    if parse_version(&system).is_none_or(|current| current < required) {
        return Err(Error::Refused(crate::tr_format!(
            "This release requires macOS {version} or later.",
            version = minimum
        )));
    }
    Ok(app)
}

fn release_version(archive: &Path) -> Result<(&str, [u32; 3])> {
    let (arch, _) = RELEASE_ARCH;
    let suffix = format!("-macOS-{arch}.zip");
    archive
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.strip_prefix("PawCommander-"))
        .and_then(|name| name.strip_suffix(&suffix))
        .and_then(|version| parse_version(version).map(|parsed| (version, parsed)))
        .ok_or_else(|| {
            Error::Refused(crate::tr_format!(
                "Use the original PawCommander macOS ZIP filename for {arch}.",
                arch = arch
            ))
        })
}

fn parse_version(value: &str) -> Option<[u32; 3]> {
    let mut parts = value.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next().map(str::parse).transpose().ok()?.unwrap_or(0);
    parts.next().is_none().then_some([major, minor, patch])
}

fn require_architecture(path: &Path) -> Result<()> {
    let (arch, cpu) = RELEASE_ARCH;
    let c_path =
        CString::new(path.as_os_str().as_bytes()).expect("existing bundle path has no NUL");
    // SAFETY: c_path remains live and NUL-terminated through this call.
    let url = unsafe {
        NSURL::fileURLWithFileSystemRepresentation_isDirectory_relativeToURL(
            NonNull::new(c_path.as_ptr().cast_mut()).expect("CString has a non-null pointer"),
            true,
            None,
        )
    };
    let matches = NSBundle::bundleWithURL(&url)
        .and_then(|bundle| bundle.executableArchitectures())
        .is_some_and(|architectures| {
            architectures
                .iter()
                .any(|entry| entry.unsignedIntValue() == cpu)
        });
    if matches {
        Ok(())
    } else {
        Err(Error::Refused(crate::tr_format!(
            "This update requires a bundle for {arch}.",
            arch = arch
        )))
    }
}

fn require_closed(path: &Path) -> Result<()> {
    unsafe extern "C" {
        fn proc_listpidspath(
            kind: u32,
            type_info: u32,
            path: *const libc::c_char,
            flags: u32,
            pids: *mut libc::c_void,
            capacity: libc::c_int,
        ) -> libc::c_int;
    }
    const ALL_PROCESSES: u32 = 1;
    const EXCLUDE_EVENT_WATCHERS: u32 = 2;

    let executable = path.join("Contents/MacOS/pawcommander");
    let c_path = CString::new(executable.as_os_str().as_bytes()).expect("app path has no NUL");
    let mut pids: [libc::pid_t; 8] = [0; 8];
    // Check open and mapped references, including launches directly from Terminal.
    // SAFETY: Both the C string and the writable buffer remain live for this call.
    let bytes = unsafe {
        proc_listpidspath(
            ALL_PROCESSES,
            0,
            c_path.as_ptr(),
            EXCLUDE_EVENT_WATCHERS,
            pids.as_mut_ptr().cast(),
            size_of_val(&pids) as libc::c_int,
        )
    };
    if bytes < 0 {
        let error = io::Error::last_os_error();
        // A damaged bundle with a missing executable can still be restored.
        return if error.kind() == io::ErrorKind::NotFound {
            Ok(())
        } else {
            at(&executable, Err(error))
        };
    }
    let own_pid = std::process::id() as libc::pid_t;
    if pids.iter().any(|pid| *pid > 0 && *pid != own_pid) {
        return Err(Error::Refused(crate::tr_format!(
            "Quit PawCommander at {path:?} before continuing.",
            path = path
        )));
    }
    Ok(())
}

fn check_bundle(path: &Path, metadata: &Metadata) -> Result<()> {
    if !metadata.is_dir() || bundle_property(path, "CFBundleIdentifier")? != APP_ID {
        return Err(invalid_bundle(path));
    }
    Ok(())
}

fn verify_bundle(path: &Path) -> Result<()> {
    check_bundle(path, &at(path, fs::symlink_metadata(path))?)?;
    run(Command::new("/usr/bin/codesign")
        .args(["--verify", "--deep", "--strict", "-R"])
        .arg(format!("=identifier {APP_ID:?}"))
        .arg(path))?;
    Ok(())
}

fn bundle_property(path: &Path, name: &str) -> Result<String> {
    run(Command::new("/usr/libexec/PlistBuddy")
        .arg("-c")
        .arg(format!("Print :{name}"))
        .arg(path.join("Contents/Info.plist")))
}

fn copy_item(source: &Path, destination: &Path) -> Result<()> {
    run(Command::new("/usr/bin/ditto")
        .arg("--qtn")
        .arg(source)
        .arg(destination))?;
    Ok(())
}

fn temporary_in(directory: &Path, prefix: &str) -> Result<TempDir> {
    at(
        directory,
        Builder::new()
            .prefix(prefix)
            .permissions(fs::Permissions::from_mode(0o700))
            .tempdir_in(directory),
    )
}

fn same_file(original: &Metadata, current: &Metadata) -> bool {
    current.is_dir() && original.dev() == current.dev() && original.ino() == current.ino()
}

fn absolute(path: &Path) -> Result<PathBuf> {
    at(path, std::path::absolute(path))
}

fn at<T>(path: &Path, result: io::Result<T>) -> Result<T> {
    result.map_err(|source| Error::Path {
        path: path.to_owned(),
        source,
    })
}

fn invalid_bundle(path: &Path) -> Error {
    Error::Refused(crate::tr_format!(
        "Expected a PawCommander app bundle at {path:?}.",
        path = path
    ))
}

fn invalid_backup(path: &Path) -> Error {
    Error::Refused(crate::tr_format!(
        "Use the saved backup path at {path:?}.",
        path = path
    ))
}

fn run(command: &mut Command) -> Result<String> {
    let program = command.get_program().to_owned();
    let output = at(Path::new(&program), command.output())?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_owned())
    } else {
        let diagnostic = if output.stderr.is_empty() {
            &output.stdout
        } else {
            &output.stderr
        };
        Err(Error::Command {
            program,
            status: output.status,
            diagnostic: String::from_utf8_lossy(diagnostic).trim().to_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_and_backup_are_private_from_creation() {
        let parent = tempfile::tempdir().unwrap();
        for prefix in [".pawcommander-update-", BACKUP_PREFIX] {
            let directory = temporary_in(parent.path(), prefix).unwrap();
            assert_eq!(fs::metadata(directory.path()).unwrap().mode() & 0o077, 0);
        }
    }
}
