mod baseline;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(in crate::vfs) use tests::two_way_round_trip;

use super::{check_new_name, observe_folders, revalidate_folders, Kind, Snapshot, SyncChange};
use crate::{
    i18n::text,
    operations::{Cancellation, Progress, TransferError},
    vfs::{transfer::validate_source_child_name, Location},
};
use baseline::BaselineStore;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Path, PathBuf},
};

type Tree = BTreeMap<PathBuf, Kind>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReconcileConflict {
    FirstComparison,
    BothChanged,
    EditAndDelete,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Reconcile {
    Change {
        destination: usize,
        change: SyncChange,
    },
    Conflict(ReconcileConflict),
}

pub(crate) struct TwoWayRow {
    pub(crate) path: PathBuf,
    pub(crate) outcome: Reconcile,
}

pub(crate) struct TwoWayPlan {
    folders: [Location; 2],
    observed: [Snapshot; 2],
    store: BaselineStore,
    baseline: Option<Tree>,
    pub(crate) rows: Vec<TwoWayRow>,
    pub(crate) conflicts: usize,
}

impl TwoWayPlan {
    pub(crate) fn build(
        folders: [Location; 2],
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let data =
            dirs::data_local_dir().ok_or_else(|| text("Папка данных приложения недоступна"))?;
        Self::build_in(folders, data.join("PawCommander/sync-baselines"), progress)
    }

    fn build_in(
        folders: [Location; 2],
        directory: PathBuf,
        progress: &mut Progress,
    ) -> Result<Self, TransferError> {
        let cancel = progress.cancellation();
        let (_, observed) = observe_folders(&folders, progress)?;
        let store = BaselineStore::new(directory, &observed)?;
        let baseline = store.read(&cancel)?;
        let (rows, conflicts) = reconcile(&observed, baseline.as_ref(), &cancel)?;
        Ok(Self {
            folders,
            observed,
            store,
            baseline,
            rows,
            conflicts,
        })
    }

    pub(crate) fn has_baseline(&self) -> bool {
        self.baseline.is_some()
    }

    pub(crate) fn can_remember(&self) -> bool {
        !self.has_baseline() && self.rows.is_empty()
    }

    pub(crate) fn verify(&self, progress: &mut Progress) -> Result<(), TransferError> {
        let cancel = progress.cancellation();
        revalidate_folders(&self.folders, &self.observed, progress)?;
        self.store.check_roots(&self.observed)?;
        if self.store.read(&cancel)? != self.baseline {
            return Err(text("Общее состояние изменилось. Постройте план заново.").into());
        }
        Ok(())
    }

    pub(crate) fn remember(&mut self, progress: &mut Progress) -> Result<(), TransferError> {
        if !self.can_remember() {
            return Err(
                text("Для первого общего состояния содержимое папок должно совпадать.").into(),
            );
        }
        self.verify(progress)?;
        let cancel = progress.cancellation();
        let common: Tree = self.observed[0]
            .entries
            .iter()
            .filter(|(path, _)| !path.as_os_str().is_empty())
            .map(|(path, entry)| (path.clone(), entry.kind.clone()))
            .collect();
        self.store.seed(&common, &cancel)?;
        self.baseline = Some(common);
        Ok(())
    }
}

fn reconcile(
    observed: &[Snapshot; 2],
    baseline: Option<&Tree>,
    cancel: &Cancellation,
) -> Result<(Vec<TwoWayRow>, usize), TransferError> {
    let [left, right] = observed;
    let union: BTreeSet<_> = left
        .entries
        .keys()
        .chain(right.entries.keys())
        .chain(baseline.into_iter().flat_map(|tree| tree.keys()))
        .collect();
    let mut paths = union
        .into_iter()
        .filter(|path| !path.as_os_str().is_empty())
        .peekable();
    let mut rows = Vec::new();
    let mut conflicts = 0;
    let mut new_names = [BTreeSet::new(), BTreeSet::new()];
    while let Some(path) = paths.next() {
        cancel.check()?;
        let current = [kind(left, path), kind(right, path)];
        if current[0] == current[1] {
            // Common directories are traversed independently, allowing new
            // files under the same directory to merge in both directions.
            continue;
        }
        let decision = decide(observed, current, baseline, path);
        let branch = std::iter::once(path).chain(std::iter::from_fn(|| {
            paths.next_if(|next| next.starts_with(path))
        }));
        match decision {
            Ok(destination) => {
                let source = 1 - destination;
                for path in branch {
                    cancel.check()?;
                    let from = kind(&observed[source], path);
                    let to = kind(&observed[destination], path);
                    let change = match (from, to) {
                        (Some(_), None) => {
                            check_new_name(
                                &observed[destination],
                                path,
                                &mut new_names[destination],
                            )?;
                            SyncChange::Copy
                        }
                        (None, Some(_)) => SyncChange::Delete,
                        (Some(from), Some(to)) if from != to => SyncChange::Replace,
                        _ => continue,
                    };
                    if left.root.remote() || right.root.remote() {
                        for name in path.iter() {
                            validate_source_child_name(&observed[source].root, name)?;
                        }
                    }
                    rows.push(TwoWayRow {
                        path: path.clone(),
                        outcome: Reconcile::Change {
                            destination,
                            change,
                        },
                    });
                }
            }
            Err(reason) => {
                conflicts += 1;
                rows.push(TwoWayRow {
                    path: path.clone(),
                    outcome: Reconcile::Conflict(reason),
                });
                // A structural conflict owns the branch. Its descendants
                // must not yield automatic writes against the disputed root.
                for _ in branch {
                    cancel.check()?;
                }
            }
        }
    }
    Ok((rows, conflicts))
}

fn kind<'a>(snapshot: &'a Snapshot, path: &Path) -> Option<&'a Kind> {
    snapshot.entries.get(path).map(|entry| &entry.kind)
}

fn decide(
    observed: &[Snapshot; 2],
    current: [Option<&Kind>; 2],
    baseline: Option<&Tree>,
    path: &Path,
) -> Result<usize, ReconcileConflict> {
    let Some(baseline) = baseline else {
        return match current {
            [None, Some(_)] => Ok(0),
            [Some(_), None] => Ok(1),
            _ => Err(ReconcileConflict::FirstComparison),
        };
    };
    let matches_previous = |snapshot: &Snapshot| {
        snapshot
            .subtree(path)
            .map(|(path, entry)| (path, &entry.kind))
            .eq(baseline
                .range(path.to_owned()..)
                .take_while(|(candidate, _)| candidate.starts_with(path)))
    };
    match [
        matches_previous(&observed[0]),
        matches_previous(&observed[1]),
    ] {
        [true, false] => Ok(0),
        [false, true] => Ok(1),
        _ if current.contains(&None) => Err(ReconcileConflict::EditAndDelete),
        _ => Err(ReconcileConflict::BothChanged),
    }
}
