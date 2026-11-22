use super::{
    archive_create::ArchivePlan,
    location::{validate_name, Location},
    staged_file::StagedFile,
    transfer::{canonical, entry_metadata, publish_replacement, with_staging_typed, EntryMetadata},
    undo::Parent,
};
use crate::{
    archive::{ArchiveWriter, EditableArchive, Format, MemberEdit},
    file_error::{AccessAction, AccessError, ArchiveError, MutationError},
    operations::{Progress, TransferError},
    result_history::Outcome,
};
use std::{
    fs,
    path::{Path, PathBuf},
};
#[cfg(target_os = "macos")]
use {super::archive_smb, crate::file_error::SmbMutationAction};

pub(crate) enum ArchiveEdit {
    Add {
        folder: PathBuf,
        sources: Vec<Location>,
    },
    Remove(Vec<PathBuf>),
    Rename {
        entry: PathBuf,
        name: String,
    },
}

pub(crate) fn edit_archive(
    file: &Path,
    edit: ArchiveEdit,
    password: Option<&str>,
    progress: &mut Progress,
) -> Result<Location, TransferError> {
    let source = Location::Local(file.to_owned());
    progress.results.plan([(source.clone(), None)]);
    progress.results.begin(&source);
    progress.item(file);
    let mut published = false;
    let result = rebuild(
        &source,
        edit,
        password.filter(|password| !password.is_empty()),
        progress,
        &mut published,
    );
    let outcome = match &result {
        Ok(()) => Outcome::Completed,
        Err(_) if published => Outcome::CompletedWithWarning,
        Err(TransferError::Cancelled) => Outcome::Cancelled,
        Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
        Err(TransferError::Failed(_)) => Outcome::Failed,
    };
    progress
        .results
        .record_with_error(&source, outcome, result.as_ref().err());
    if outcome == Outcome::Cancelled {
        progress.results.cancelled = true;
    }
    result.map(|()| source)
}

fn rebuild(
    source: &Location,
    edit: ArchiveEdit,
    password: Option<&str>,
    progress: &mut Progress,
    published: &mut bool,
) -> Result<(), TransferError> {
    let cancel = progress.cancellation();
    cancel.check()?;
    let format = Format::from_path(source.path())
        .ok_or_else(|| ArchiveError::UnsupportedFormat(source.clone()))?;
    let changed = || ArchiveError::SourceChanged(source.clone());
    let directory = source.parent().ok_or_else(changed)?;
    #[cfg(target_os = "macos")]
    let mut mount = archive_smb::guard(source, SmbMutationAction::EditArchive)?;
    let parent = Parent::capture(&directory)?;
    let target = parent
        .location
        .join(source.path().file_name().ok_or_else(changed)?);
    #[cfg(target_os = "macos")]
    if archive_smb::is_destination(&mut mount, &target)? {
        return Err(ArchiveError::SmbEditingUnavailable(target).into());
    }
    let expected = entry_metadata(&target)?
        .filter(EntryMetadata::is_file)
        .ok_or_else(changed)?;

    let moved_to;
    let edit_paths = match &edit {
        ArchiveEdit::Add { folder, .. } => MemberEdit::Add(folder),
        ArchiveEdit::Remove(paths) => {
            MemberEdit::Remove(paths.iter().map(PathBuf::as_path).collect())
        }
        ArchiveEdit::Rename { entry, name } => {
            let name = name.trim();
            validate_name(name)?;
            moved_to = entry.with_file_name(name);
            if moved_to == *entry {
                let location = source
                    .browse_archive()
                    .expect("supported archive format")
                    .join(entry);
                return Err(ArchiveError::MemberNameUnchanged(location).into());
            }
            MemberEdit::Rename {
                from: entry,
                to: &moved_to,
            }
        }
    };
    let archive = EditableArchive::open(target.path(), format, &edit_paths, progress)?;
    let mut additions = ArchivePlan::new(format, password.is_some());
    let additions_origin = if let ArchiveEdit::Add { folder, sources } = &edit {
        let origin = sources
            .first()
            .and_then(Location::parent)
            .filter(|origin| {
                !origin.is_archive()
                    && sources
                        .iter()
                        .all(|source| source.parent().as_ref() == Some(origin))
            })
            .ok_or(ArchiveError::SelectOtherPane)?;
        #[cfg(target_os = "macos")]
        additions.check_source(&origin)?;
        let origin_parent = Parent::capture(&origin)?;
        additions.capture_sources(sources, &origin_parent.location, folder, progress, &cancel)?;
        if additions.contains_file(&expected) {
            return Err(ArchiveError::CannotAddArchiveToItself(source.clone()).into());
        }
        if archive.has_protected_entries()
            && password.is_none()
            && additions.names().any(|(_, dir)| !dir)
        {
            return Err(ArchiveError::AdditionsRequirePassword(source.clone()).into());
        }
        Some((origin, origin_parent))
    } else {
        None
    };
    archive.validate_additions(additions.names())?;
    with_staging_typed(&target, Some(&cancel), |staging, preserve| {
        let payload = staging.join("payload");
        let output = StagedFile::open(&payload, cancel.clone())?;
        let mut writer = ArchiveWriter::rebuild(format, password, output)
            .map_err(|error| AccessError::write(source, error))?;
        archive.write_existing(&mut writer, progress)?;
        additions.append(source, &mut writer, progress, &cancel)?;
        progress.item(source.path());
        writer
            .finish()
            .map_err(|error| AccessError::write(source, error))?
            .close()?;
        archive.validate_output(payload.path(), additions.names(), progress)?;
        if let EntryMetadata::Local(metadata) = &expected {
            fs::set_permissions(payload.path(), metadata.permissions())
                .map_err(|error| AccessError::write(&payload, error))?;
        }
        additions.verify(&cancel)?;
        if let Some((origin, captured)) = &additions_origin {
            captured.verify()?;
            if canonical(origin)? != captured.location.path() {
                return Err(MutationError::SourceChanged(origin.clone()).into());
            }
        }
        archive.verify_source()?;
        if let EntryMetadata::Local(metadata) = &expected {
            let metadata_error =
                |error| AccessError::new(&target, AccessAction::ReadMetadata, error);
            let current = fs::symlink_metadata(target.path()).map_err(metadata_error)?;
            if !crate::fs_ops::source_metadata_matches(metadata, &current)
                .map_err(metadata_error)?
            {
                return Err(changed().into());
            }
        }
        parent.verify()?;
        if canonical(&directory)? != parent.location.path() {
            return Err(changed().into());
        }
        cancel.check()?;
        publish_replacement(&payload, &target, staging, &expected, preserve)?;
        *published = true;
        Ok(())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{file_error::Failure, operations::Cancellation, vfs::create_archive};
    use std::{
        collections::BTreeMap,
        io::{self, Read, Write},
    };

    #[test]
    fn adds_renames_and_removes_records_in_zip_and_tar() {
        let temp = tempfile::tempdir().unwrap();
        let root = Location::Local(temp.path().into());
        let folder = root.join("bundle");
        let peer = root.join("peer");
        fs::create_dir_all(folder.join("old-empty").path()).unwrap();
        fs::create_dir_all(peer.join("added/empty").path()).unwrap();
        fs::write(folder.join("keep.txt").path(), b"keep these bytes").unwrap();
        fs::write(folder.join("drop.txt").path(), b"remove this entry").unwrap();
        fs::write(peer.join("added/new.txt").path(), b"new file bytes").unwrap();

        for format in [Format::Zip, Format::Tar] {
            let path = temp.path().join(format!("bundle.{}", format.extension()));
            let mut progress = Progress::default();
            create_archive(
                std::slice::from_ref(&folder),
                &root,
                "bundle",
                format,
                None,
                &mut progress,
            )
            .unwrap();
            let add = ArchiveEdit::Add {
                folder: "bundle".into(),
                sources: vec![peer.join("added")],
            };
            edit_archive(&path, add, None, &mut progress).unwrap();
            edit_archive(
                &path,
                ArchiveEdit::Rename {
                    entry: "bundle".into(),
                    name: "новое".into(),
                },
                None,
                &mut progress,
            )
            .unwrap();
            edit_archive(
                &path,
                ArchiveEdit::Remove(vec!["новое/drop.txt".into(), "новое/old-empty".into()]),
                None,
                &mut progress,
            )
            .unwrap();
            assert_eq!(
                read_files(&path, format),
                BTreeMap::from([
                    ("новое/added/new.txt".into(), b"new file bytes".to_vec()),
                    ("новое/keep.txt".into(), b"keep these bytes".to_vec()),
                ])
            );
            let tree = crate::archive::Index::load(&path, &|| false).unwrap();
            let additions = Location::Local(path.clone())
                .browse_archive()
                .unwrap()
                .join("новое/added");
            let entries = tree.list(&additions, &|| false).unwrap();
            assert!(entries
                .iter()
                .any(|entry| entry.name == "empty" && entry.is_dir));
            assert_eq!(progress.results.items[0].outcome, Outcome::Completed);
            assert_eq!(
                fs::read(peer.join("added/new.txt").path()).unwrap(),
                b"new file bytes"
            );
            assert!(!has_stage(temp.path()));
        }
    }

    #[test]
    fn existing_encrypted_zip_payload_stays_identical_on_rename_and_add() {
        let temp = tempfile::tempdir().unwrap();
        let root = Location::Local(temp.path().into());
        let source = root.join("secret.txt");
        let addition = root.join("new.txt");
        fs::write(source.path(), b"old encrypted content").unwrap();
        fs::write(addition.path(), b"new encrypted content").unwrap();
        let old_password = "disposable existing password";
        let new_password = "disposable addition password";
        let mut progress = Progress::default();
        create_archive(
            &[source],
            &root,
            "locked",
            Format::Zip,
            Some(old_password),
            &mut progress,
        )
        .unwrap();
        let archive_path = temp.path().join("locked.zip");
        let before = zip_payload(&archive_path);
        edit_archive(
            &archive_path,
            ArchiveEdit::Rename {
                entry: "secret.txt".into(),
                name: "renamed.txt".into(),
            },
            None,
            &mut progress,
        )
        .unwrap();
        assert_eq!(zip_payload(&archive_path), before);

        let unchanged = fs::read(&archive_path).unwrap();
        let add = || ArchiveEdit::Add {
            folder: PathBuf::new(),
            sources: vec![addition.clone()],
        };
        let error = edit_archive(&archive_path, add(), None, &mut progress).unwrap_err();
        assert!(
            matches!(
                &error,
                TransferError::Failed(Failure::Archive(reason))
                    if matches!(reason.as_ref(), ArchiveError::AdditionsRequirePassword(location)
                        if location.path() == archive_path)
            ),
            "{error:?}"
        );
        assert_eq!(fs::read(&archive_path).unwrap(), unchanged);
        edit_archive(&archive_path, add(), Some(new_password), &mut progress).unwrap();
        assert_eq!(zip_payload(&archive_path), before);
        let mut archive = zip::ZipArchive::new(fs::File::open(archive_path).unwrap()).unwrap();
        for (name, password, expected) in [
            ("renamed.txt", old_password, b"old encrypted content"),
            ("new.txt", new_password, b"new encrypted content"),
        ] {
            let mut contents = Vec::new();
            archive
                .by_name_decrypt(name, password.as_bytes())
                .unwrap()
                .read_to_end(&mut contents)
                .unwrap();
            assert_eq!(&contents, expected);
        }
    }

    #[test]
    fn tar_rename_preserves_pax_metadata_and_retargets_hard_links() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("links.tar");
        let old_path = format!("original/{}.txt", "long-name-".repeat(20));
        let mut tar = tar::Builder::new(fs::File::create(&path).unwrap());
        tar.append_pax_extensions([
            ("path", old_path.as_bytes()),
            ("SCHILY.xattr.user.fixture", b"same metadata" as &[u8]),
        ])
        .unwrap();
        let mut header = tar::Header::new_ustar();
        header.set_size(4);
        header.set_mode(0o640);
        tar.append_data(&mut header, "fallback", &b"data"[..])
            .unwrap();
        let mut header = tar::Header::new_ustar();
        header.set_size(0);
        header.set_entry_type(tar::EntryType::Link);
        tar.append_link(&mut header, "hardlink", &old_path).unwrap();
        drop(tar.into_inner().unwrap());

        let original = fs::read(&path).unwrap();
        let mut progress = Progress::default();
        let error = edit_archive(
            &path,
            ArchiveEdit::Remove(vec!["original".into()]),
            None,
            &mut progress,
        )
        .unwrap_err();
        assert!(
            crate::i18n::file_error(&error).contains("hard link"),
            "{error:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), original);

        edit_archive(
            &path,
            ArchiveEdit::Rename {
                entry: "original".into(),
                name: "renamed".into(),
            },
            None,
            &mut progress,
        )
        .unwrap();
        let mut tar = tar::Archive::new(fs::File::open(&path).unwrap());
        let mut entries = tar.entries().unwrap();
        let mut target = entries.next().unwrap().unwrap();
        let renamed_path = old_path.replacen("original/", "renamed/", 1);
        assert_eq!(target.path_bytes(), renamed_path.as_bytes());
        assert_eq!(target.header().mode().unwrap(), 0o640);
        assert!(target.pax_extensions().unwrap().unwrap().any(|extension| {
            extension.is_ok_and(|field| {
                field.key_bytes() == b"SCHILY.xattr.user.fixture"
                    && field.value_bytes() == b"same metadata"
            })
        }));
        let mut bytes = Vec::new();
        target.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"data");
        assert_eq!(
            entries.next().unwrap().unwrap().link_name_bytes().unwrap(),
            renamed_path.as_bytes()
        );
        assert!(entries.next().is_none());
    }

    #[test]
    fn refuses_to_merge_an_occupied_implicit_zip_folder() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("occupied.zip");
        let mut zip = zip::ZipWriter::new(fs::File::create(&path).unwrap());
        for name in ["first/one.txt", "second/two.txt"] {
            zip.start_file(name, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(b"keep this").unwrap();
        }
        drop(zip.finish().unwrap());
        let before = fs::read(&path).unwrap();
        let mut progress = Progress::default();
        let error = edit_archive(
            &path,
            ArchiveEdit::Rename {
                entry: "first".into(),
                name: "second".into(),
            },
            None,
            &mut progress,
        )
        .unwrap_err();
        assert!(
            matches!(
                &error,
                TransferError::Failed(Failure::Archive(reason))
                    if matches!(reason.as_ref(), ArchiveError::MemberOccupied(location)
                        if location.archive_source() == Some(path.canonicalize().unwrap().as_path())
                            && location.path() == Path::new("second"))
            ),
            "{error:?}"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!has_stage(temp.path()));
    }

    #[test]
    fn cancellation_and_concurrent_replacement_leave_archive_bytes_untouched() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("active.tar");
        let mut tar = tar::Builder::new(fs::File::create(&path).unwrap());
        let mut header = tar::Header::new_gnu();
        let size = 2 * 1024 * 1024;
        header.set_size(size);
        tar.append_data(&mut header, "large.bin", io::repeat(0xab).take(size))
            .unwrap();
        drop(tar.into_inner().unwrap());
        let initial = fs::read(&path).unwrap();
        let rename = || ArchiveEdit::Rename {
            entry: "large.bin".into(),
            name: "different.bin".into(),
        };
        let directory = temp.path().to_owned();
        let mut progress = Progress::with_cancellation(Cancellation::at_checkpoint(move || {
            has_payload(&directory)
        }));
        let cancelled = edit_archive(&path, rename(), None, &mut progress).unwrap_err();
        assert!(matches!(cancelled, TransferError::Cancelled));
        assert_eq!(progress.results.items[0].outcome, Outcome::Cancelled);
        assert_eq!(fs::read(&path).unwrap(), initial);
        assert!(!has_stage(temp.path()));

        let directory = temp.path().to_owned();
        let target = path.clone();
        let original = temp.path().join("original-backup.tar");
        let replacement = temp.path().join("replacement.tar");
        let mut tar = tar::Builder::new(fs::File::create(&replacement).unwrap());
        tar.finish().unwrap();
        drop(tar);
        let replacement_bytes = fs::read(&replacement).unwrap();
        let mut displaced = false;
        let saved = original.clone();
        let mut progress = Progress::with_cancellation(Cancellation::at_checkpoint(move || {
            if !displaced && has_payload(&directory) {
                fs::rename(&target, &saved).unwrap();
                fs::rename(&replacement, &target).unwrap();
                displaced = true;
            }
            false
        }));
        assert!(edit_archive(&path, rename(), None, &mut progress).is_err());
        assert_eq!(progress.results.items[0].outcome, Outcome::Failed);
        assert_eq!(fs::read(&path).unwrap(), replacement_bytes);
        assert_eq!(fs::read(original).unwrap(), initial);
        assert!(!has_stage(temp.path()));
    }

    fn read_files(path: &Path, format: Format) -> BTreeMap<String, Vec<u8>> {
        let file = fs::File::open(path).unwrap();
        let mut files = BTreeMap::new();
        match format {
            Format::Zip => {
                let mut zip = zip::ZipArchive::new(file).unwrap();
                for index in 0..zip.len() {
                    let mut entry = zip.by_index(index).unwrap();
                    if !entry.is_dir() {
                        let mut bytes = Vec::new();
                        entry.read_to_end(&mut bytes).unwrap();
                        files.insert(entry.name().unwrap().into_owned(), bytes);
                    }
                }
            }
            Format::Tar => {
                let mut tar = tar::Archive::new(file);
                for entry in tar.entries().unwrap() {
                    let mut entry = entry.unwrap();
                    if entry.header().entry_type().is_file() {
                        let mut bytes = Vec::new();
                        entry.read_to_end(&mut bytes).unwrap();
                        files.insert(entry.path().unwrap().to_string_lossy().into_owned(), bytes);
                    }
                }
            }
        }
        files
    }

    fn zip_payload(path: &Path) -> Vec<u8> {
        let bytes = fs::read(path).unwrap();
        let archive = rawzip::ZipArchive::from_slice(&bytes).unwrap();
        let mut entries = archive.entries();
        let entry = entries.next_entry().unwrap().unwrap();
        archive
            .get_entry(entry.wayfinder())
            .unwrap()
            .data()
            .to_owned()
    }

    fn staging(path: &Path) -> impl Iterator<Item = PathBuf> {
        fs::read_dir(path).unwrap().filter_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .starts_with(".pawcommander-transfer-")
                .then(|| entry.path())
        })
    }

    fn has_stage(path: &Path) -> bool {
        staging(path).next().is_some()
    }

    fn has_payload(path: &Path) -> bool {
        staging(path)
            .any(|stage| fs::metadata(stage.join("payload")).is_ok_and(|meta| meta.len() > 0))
    }
}
