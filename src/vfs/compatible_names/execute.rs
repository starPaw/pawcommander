use super::{
    metadata_name, names_in, Cancellation, Location, Plan, Progress, Root, TransferError,
    TransferKind,
};
use crate::{
    operations::CopyReader,
    result_history::Outcome,
    vfs::{
        copy_metadata,
        cross_move::{self, MoveSource},
        guarded_directory::{open_staging, Directory},
        transfer::{
            check_copy_space, entry_metadata, with_staging_typed, EntryMetadata,
            LocalEntryMetadata, SourceReader, TransferSourceEntry, TransferSourceManifest,
        },
    },
};
use std::{
    collections::{BTreeMap, HashSet},
    ffi::OsStr,
    io::{Read, Seek, SeekFrom},
    path::Path,
};

impl Plan {
    pub(crate) fn kind(&self) -> TransferKind {
        self.kind
    }

    pub(crate) fn destination(&self) -> &Location {
        &self.destination.location
    }

    pub(crate) fn apply(self, progress: &mut Progress) -> Result<String, String> {
        let cancel = progress.cancellation().pausable_scan();
        progress.results.plan(
            self.roots
                .iter()
                .map(|root| (root.item.original.clone(), Some(root.item.target.display()))),
        );
        if let Err(error) = self.verify(&cancel) {
            if matches!(error, TransferError::Cancelled) {
                progress.results.cancelled = true;
            }
            return Err(crate::i18n::file_error(&error));
        }
        let completed = self.roots.len();
        for (completed, root) in self.roots.into_iter().enumerate() {
            let source = root.item.original.clone();
            let target = root.item.target.clone();
            progress.results.begin(&source);
            for (entry, relative) in &root.paths {
                progress.results.capturing_target(
                    &root.item.source,
                    entry,
                    &beneath(&target, relative),
                );
            }
            progress.results.captured();
            let mut effect = Effect::default();
            let result = apply_root(
                root,
                &self.destination,
                self.kind,
                self.direct,
                self.apple_double,
                progress,
                &mut effect,
            );
            let outcome = match &result {
                Ok(()) => Outcome::Completed,
                Err(_)
                    if effect.published && self.kind == TransferKind::Move && !effect.removed =>
                {
                    Outcome::CopiedSourceRemaining
                }
                Err(_) if effect.published => Outcome::CompletedWithWarning,
                Err(TransferError::Cancelled) => Outcome::Cancelled,
                Err(TransferError::PublicationUnconfirmed(_)) => Outcome::PublicationUnconfirmed,
                Err(TransferError::Failed(_)) => Outcome::Failed,
            };
            progress.results.record(
                &source,
                Some(target.display()),
                outcome,
                result.as_ref().err().map(crate::i18n::file_error),
            );
            if let Err(error) = result {
                if matches!(error, TransferError::Cancelled) {
                    progress.results.cancelled = true;
                }
                return Err(crate::tr_format!(
                    "Completed items: {completed}. {error}",
                    completed = completed,
                    error = crate::i18n::file_error(&error)
                ));
            }
        }
        Ok(crate::tr_format!(
            "Transfer with compatible names finished. Items: {completed}",
            completed = completed
        ))
    }

    fn verify(&self, cancel: &Cancellation) -> Result<(), TransferError> {
        let occupied = names_in(&self.destination, cancel)?;
        for root in &self.roots {
            check_target(&root.item.target, &occupied, self.apple_double)?;
            root.manifest.verify(cancel, |_, _| {})?;
        }
        Ok(())
    }
}

#[derive(Default)]
struct Effect {
    published: bool,
    removed: bool,
}

fn apply_root(
    root: Root,
    destination: &Directory,
    kind: TransferKind,
    direct: bool,
    apple_double: bool,
    progress: &mut Progress,
    effect: &mut Effect,
) -> Result<(), TransferError> {
    progress.checkpoint()?;
    let cancel = progress.cancellation();
    let target = &root.item.target;
    let name = target.path().file_name().expect("reviewed target");
    check_target(target, &names_in(destination, &cancel)?, apple_double)?;
    progress.set_copy_endpoints(&root.item.source, target);
    let mut copy = CopyTree {
        root: &root,
        progress,
        check_bytes: kind == TransferKind::Move,
        apple_double,
        identities: BTreeMap::new(),
        created: false,
    };
    if direct {
        // exFAT permits exclusive creation but refuses exclusive rename to a
        // vacant name. Its visible result retains the source until verified.
        check_copy_space(root.manifest.bytes, target, &destination.location)?;
        let result = copy
            .write(&root.item.source, destination, name)
            .and_then(|()| root.manifest.verify(&cancel, |_, _| {}));
        if let Err(error) = result {
            if copy.created || matches!(error, TransferError::PublicationUnconfirmed(_)) {
                return Err(unconfirmed(target, error));
            }
            return Err(error);
        }
    } else {
        let mut renamed = false;
        let publication = with_staging_typed(target, Some(&cancel), |stage, _| {
            let stage = Directory::from_scope(&open_staging(stage)?)?;
            check_copy_space(root.manifest.bytes, target, &stage.location)?;
            let payload = stage.location.join("payload");
            copy.write(&root.item.source, &stage, OsStr::new("payload"))
                .map_err(|error| match error {
                    // Only the private draft could have been created.
                    TransferError::PublicationUnconfirmed(reason) => TransferError::Failed(reason),
                    error => error,
                })?;
            root.manifest.verify(&cancel, |_, _| {})?;
            copy.verify_output(&payload, &cancel)?;
            check_target(target, &names_in(destination, &cancel)?, apple_double)?;
            copy.progress.checkpoint()?;
            stage.rename(OsStr::new("payload"), destination, name)?;
            renamed = true;
            Ok(())
        });
        publication.map_err(|error| {
            if renamed {
                unconfirmed(target, error)
            } else {
                error
            }
        })?;
    }
    let (output, paths) = copy
        .verify_output(target, &Cancellation::default())
        .map_err(|error| unconfirmed(target, error))?;
    verify_published_name(destination, target, apple_double)
        .map_err(|error| unconfirmed(target, error))?;
    effect.published = true;
    let progress = copy.progress;
    match kind {
        TransferKind::Copy => {
            for source in paths.keys() {
                progress.results.moved_entry(source, true, None);
            }
            finish_attributes(&root, &output, &paths, progress)
        }
        TransferKind::Move => {
            progress.results.copied_source_remaining();
            let source = MoveSource::from_manifest(&root.item, root.manifest)
                .map_err(|error| cross_move::incomplete(target, error, progress))?;
            cross_move::remove_verified_at(
                &root.item,
                source,
                Some(output),
                Some(&paths),
                progress,
                || effect.removed = true,
                &mut |_, _| Ok(()),
            )
        }
    }
}

struct CopyTree<'source, 'work> {
    root: &'source Root,
    progress: &'work mut Progress,
    check_bytes: bool,
    apple_double: bool,
    identities: BTreeMap<Location, EntryMetadata>,
    created: bool,
}

impl CopyTree<'_, '_> {
    fn write(
        &mut self,
        source: &Location,
        parent: &Directory,
        name: &OsStr,
    ) -> Result<(), TransferError> {
        self.progress.checkpoint()?;
        self.progress.item(source.path());
        let cancel = self.progress.cancellation();
        let entry = &self.root.manifest.entries[source];
        entry.verify(source, &cancel, true)?;
        if self.apple_double {
            // macOS may create the companion during open, even when the
            // caller never writes any extended attributes.
            parent.verify_absent(&metadata_name(name))?;
        }
        let result = if let Some(children) = &entry.children {
            let directory = parent.exclusive_directory(name)?;
            self.created = true;
            for child in children {
                let child = source.join(child);
                let output = self.root.paths[&child]
                    .file_name()
                    .expect("planned child name");
                self.write(&child, &directory, output)?;
            }
            directory.current_metadata()
        } else {
            let mut input = SourceReader::open(source)?;
            entry.verify_metadata(source, &input.metadata(source)?)?;
            let mut file = parent.exclusive_file(name)?;
            self.created = true;
            let size = entry.size();
            let verify = self.check_bytes || self.progress.copy_verification();
            let mut data = CopyReader::new((&mut input).take(size), verify);
            let written = self
                .progress
                .copy(source.path(), Some(size), &mut data, &mut file)?;
            file.sync_all()?;
            if written != size || file.metadata()?.len() != size {
                return Err(stale_source(source));
            }
            if let Some(hash) = data.finish() {
                file.seek(SeekFrom::Start(0))?;
                self.progress
                    .verify_copy(source.path(), size, hash, &mut file)?;
            }
            entry.verify_metadata(source, &input.metadata(source)?)?;
            entry.verify(source, &cancel, true)?;
            Ok(EntryMetadata::Local(LocalEntryMetadata::from_file(
                &file,
                file.metadata()?,
            )))
        };
        let identity = result?;
        self.progress.results.staged(source, None, false);
        self.identities.insert(source.clone(), identity);
        Ok(())
    }

    fn verify_output(
        &self,
        target: &Location,
        cancel: &Cancellation,
    ) -> Result<(TransferSourceManifest, BTreeMap<Location, Location>), TransferError> {
        let output = TransferSourceManifest::capture_move_observed(target, cancel, &mut |_, _| {})?;
        let mut by_name: BTreeMap<_, _> = output
            .entries
            .iter()
            .map(|(location, entry)| {
                let relative = location
                    .path()
                    .strip_prefix(target.path())
                    .expect("output tree");
                (key(relative), (location, entry))
            })
            .collect();
        if by_name.len() != output.entries.len() {
            return Err(stale_target(target));
        }
        let mut companions = HashSet::new();
        let mut paths = BTreeMap::new();
        for (source, relative) in &self.root.paths {
            cancel.check()?;
            let (path, actual) = by_name
                .remove(&key(relative))
                .ok_or_else(|| stale_target(&beneath(target, relative)))?;
            let created = TransferSourceEntry {
                metadata: self.identities[source].clone(),
                children: None,
            };
            if !created.matches_moved_target(&actual.metadata) {
                return Err(stale_target(path));
            }
            if self.apple_double
                && let Some(name) = relative.file_name()
            {
                companions.insert(key(&relative.with_file_name(metadata_name(name))));
            }
            paths.insert(source.clone(), path.clone());
        }
        for (relative, (path, entry)) in by_name {
            cancel.check()?;
            if !companions.contains(&relative) {
                return Err(stale_target(path));
            }
            verify_apple_double(path, entry)?;
        }
        Ok((output, paths))
    }
}

fn key(relative: &Path) -> Vec<String> {
    relative
        .components()
        .map(|part| super::portable_key(part.as_os_str()))
        .collect()
}

fn finish_attributes(
    root: &Root,
    output: &TransferSourceManifest,
    paths: &BTreeMap<Location, Location>,
    progress: &mut Progress,
) -> Result<(), TransferError> {
    let cancel = Cancellation::default();
    for (source, entry) in root.manifest.entries.iter().rev() {
        let target = &paths[source];
        let copy = &output.entries[target];
        copy.verify(target, &cancel, true)?;
        let wanted = copy_metadata::Attributes::from_entry(&entry.metadata);
        let warning = if entry.children.is_some() {
            copy_metadata::finish_directory(target, &wanted, copy)?
        } else {
            let mut handle = SourceReader::open(target)?;
            copy.verify_metadata(target, &handle.metadata(target)?)?;
            copy_metadata::finish_file(target, &wanted, &mut handle)?
        };
        progress.results.note_metadata(source, warning.as_deref());
    }
    Ok(())
}

fn check_target(
    target: &Location,
    occupied: &HashSet<String>,
    apple_double: bool,
) -> Result<(), TransferError> {
    let name = target.path().file_name().expect("reviewed target");
    check_vacant(target, occupied)?;
    if apple_double {
        check_vacant(
            &target.with_path(target.path().with_file_name(metadata_name(name))),
            occupied,
        )?;
    }
    Ok(())
}

fn check_vacant(target: &Location, occupied: &HashSet<String>) -> Result<(), TransferError> {
    let name = target.path().file_name().expect("reviewed target");
    if !occupied.contains(&super::portable_key(name)) && entry_metadata(target)?.is_none() {
        return Ok(());
    }
    Err(crate::tr_format!(
        "Destination name became occupied after preview: {target}. Rebuild the plan.",
        target = target
    )
    .into())
}

fn verify_published_name(
    destination: &Directory,
    target: &Location,
    apple_double: bool,
) -> Result<(), TransferError> {
    destination.verify()?;
    let name = target.path().file_name().expect("reviewed target");
    let expected = super::portable_key(name);
    let companion = apple_double.then(|| super::portable_key(&metadata_name(name)));
    let mut matches = 0;
    let mut companions = 0;
    for item in std::fs::read_dir(destination.location.path())? {
        let item = item?;
        let key = super::portable_key(&item.file_name());
        if key == expected {
            matches += 1;
        } else if companion.as_ref() == Some(&key) {
            let path = destination.location.join(item.file_name());
            let metadata = entry_metadata(&path)?.ok_or_else(|| stale_target(&path))?;
            verify_apple_double(
                &path,
                &TransferSourceEntry {
                    metadata,
                    children: None,
                },
            )?;
            companions += 1;
        }
    }
    if matches != 1 || companions > 1 {
        return Err(stale_target(target));
    }
    Ok(())
}

fn verify_apple_double(path: &Location, entry: &TransferSourceEntry) -> Result<(), TransferError> {
    // AppleDouble v2, RFC 1740. Unexpected sibling files remain a hard failure.
    const HEADER: [u8; 8] = [0, 5, 0x16, 7, 0, 2, 0, 0];
    if !entry.metadata.is_file() {
        return Err(stale_target(path));
    }
    let mut file = SourceReader::open(path)?;
    entry.verify_metadata(path, &file.metadata(path)?)?;
    let mut header = [0; HEADER.len()];
    file.read_exact(&mut header)?;
    if header != HEADER {
        return Err(stale_target(path));
    }
    Ok(())
}

fn beneath(root: &Location, relative: &Path) -> Location {
    if relative.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(relative)
    }
}

fn stale_source(source: &Location) -> TransferError {
    crate::tr_format!("Source file changed: {source}", source = source).into()
}

fn stale_target(target: &Location) -> TransferError {
    crate::tr_format!(
        "Destination differs from the verified copy: {target}",
        target = target
    )
    .into()
}

fn unconfirmed(target: &Location, error: TransferError) -> TransferError {
    TransferError::publication_unconfirmed(crate::tr_format!(
        "Inspect the possibly incomplete copy at {target}. The source was preserved. {error}",
        error = crate::i18n::file_error(&error),
        target = target
    ))
}
