use crate::operations::{Cancellation, TransferError};
use std::collections::HashMap;

const MAX_LINES: usize = 30_000;
const MAX_EDIT_DISTANCE: usize = 1_536;
const CONTEXT: usize = 3;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffKind {
    Same,
    Removed,
    Added,
}

#[derive(Debug)]
pub(crate) struct DiffLine {
    pub(crate) kind: DiffKind,
    pub(crate) left: Option<usize>,
    pub(crate) right: Option<usize>,
    pub(crate) text: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DiffRow {
    Line(usize),
    Gap(usize),
}

#[derive(Debug)]
pub(crate) struct TextDifference {
    pub(crate) lines: Vec<DiffLine>,
    pub(crate) context: Vec<DiffRow>,
    pub(crate) added: usize,
    pub(crate) removed: usize,
    pub(crate) grouped: bool,
}

pub(super) fn compare(
    left: &str,
    right: &str,
    cancel: &Cancellation,
) -> Result<Option<TextDifference>, TransferError> {
    let left: Vec<_> = left.lines().take(MAX_LINES + 1).collect();
    let right: Vec<_> = right.lines().take(MAX_LINES + 1).collect();
    if left.len() > MAX_LINES || right.len() > MAX_LINES {
        return Ok(None);
    }
    let [left_ids, right_ids] = intern([&left, &right]);
    let (edits, grouped) = edit_script(&left_ids, &right_ids, cancel)?;
    let mut diff = TextDifference {
        lines: Vec::with_capacity(edits.len()),
        context: Vec::new(),
        added: 0,
        removed: 0,
        grouped,
    };
    let (mut a, mut b) = (0, 0);
    for (index, kind) in edits.into_iter().enumerate() {
        if index.is_multiple_of(256) {
            cancel.check()?;
        }
        let line = match kind {
            DiffKind::Same => {
                let text = left[a].to_owned();
                a += 1;
                b += 1;
                DiffLine {
                    kind,
                    left: Some(a),
                    right: Some(b),
                    text,
                }
            }
            DiffKind::Removed => {
                let text = left[a].to_owned();
                a += 1;
                diff.removed += 1;
                DiffLine {
                    kind,
                    left: Some(a),
                    right: None,
                    text,
                }
            }
            DiffKind::Added => {
                let text = right[b].to_owned();
                b += 1;
                diff.added += 1;
                DiffLine {
                    kind,
                    left: None,
                    right: Some(b),
                    text,
                }
            }
        };
        diff.lines.push(line);
    }
    diff.context = context_rows(&diff.lines);
    Ok(Some(diff))
}

fn intern(inputs: [&[&str]; 2]) -> [Vec<usize>; 2] {
    let mut ids = HashMap::new();
    inputs.map(|lines| {
        lines
            .iter()
            .map(|&line| {
                let next = ids.len();
                *ids.entry(line).or_insert(next)
            })
            .collect()
    })
}

fn edit_script(
    left: &[usize],
    right: &[usize],
    cancel: &Cancellation,
) -> Result<(Vec<DiffKind>, bool), TransferError> {
    use DiffKind::{Added, Removed, Same};

    cancel.check()?;
    let common = |left: &[usize], right: &[usize]| {
        left.iter().zip(right).take_while(|(a, b)| a == b).count()
    };
    let prefix = common(left, right);
    let left = &left[prefix..];
    let right = &right[prefix..];
    let suffix = left
        .iter()
        .rev()
        .zip(right.iter().rev())
        .take_while(|(a, b)| a == b)
        .count();
    let left = &left[..left.len() - suffix];
    let right = &right[..right.len() - suffix];
    let mut edits = Vec::with_capacity(left.len() + right.len() + prefix + suffix);
    edits.extend(std::iter::repeat_n(Same, prefix));
    let mut grouped = false;
    if left.is_empty() || right.is_empty() {
        edits.extend(std::iter::repeat_n(Removed, left.len()));
        edits.extend(std::iter::repeat_n(Added, right.len()));
    } else if let Some(middle) = shortest(left, right, cancel)? {
        edits.extend(middle);
    } else {
        // A dense change still has an exact edit script. Show its central
        // block together once the shortest-path search reaches its budget.
        grouped = true;
        edits.extend(std::iter::repeat_n(Removed, left.len()));
        edits.extend(std::iter::repeat_n(Added, right.len()));
    }
    edits.extend(std::iter::repeat_n(Same, suffix));
    Ok((edits, grouped))
}

/// Myers' edit graph. Store only the reachable diagonals from each round.
fn shortest(
    left: &[usize],
    right: &[usize],
    cancel: &Cancellation,
) -> Result<Option<Vec<DiffKind>>, TransferError> {
    let limit = MAX_EDIT_DISTANCE.min(left.len() + right.len()) as isize;
    let origin = limit + 1;
    let index = |diagonal| (origin + diagonal) as usize;
    let mut front = vec![-1; index(limit + 1) + 1];
    front[index(1)] = 0;
    let mut trace = Vec::new();
    let (width, height) = (left.len() as isize, right.len() as isize);
    for depth in 0..=limit {
        cancel.check()?;
        let mut round = Vec::with_capacity((depth + 1) as usize);
        for diagonal in (-depth..=depth).step_by(2) {
            let previous = parent(diagonal, depth, |k| front[index(k)]);
            let mut x = front[index(previous)] + isize::from(previous < diagonal);
            let mut y = x - diagonal;
            while x < width && y < height && left[x as usize] == right[y as usize] {
                x += 1;
                y += 1;
            }
            if x == width && y == height {
                return Ok(Some(backtrack(&trace, x, y)));
            }
            front[index(diagonal)] = x;
            round.push(x);
        }
        trace.push(round);
    }
    Ok(None)
}

fn parent(diagonal: isize, depth: isize, previous: impl Fn(isize) -> isize) -> isize {
    if diagonal == -depth || (diagonal != depth && previous(diagonal - 1) < previous(diagonal + 1))
    {
        diagonal + 1
    } else {
        diagonal - 1
    }
}

fn backtrack(trace: &[Vec<isize>], mut x: isize, mut y: isize) -> Vec<DiffKind> {
    use DiffKind::{Added, Removed, Same};

    let mut edits = Vec::new();
    for (depth, round) in trace.iter().enumerate().rev() {
        let depth = depth as isize;
        let previous = |diagonal| round[((diagonal + depth) / 2) as usize];
        let diagonal = parent(x - y, depth + 1, previous);
        let last_x = previous(diagonal);
        let last_y = last_x - diagonal;
        while x > last_x && y > last_y {
            edits.push(Same);
            x -= 1;
            y -= 1;
        }
        if x == last_x {
            edits.push(Added);
            y -= 1;
        } else {
            edits.push(Removed);
            x -= 1;
        }
    }
    edits.extend(std::iter::repeat_n(Same, x as usize));
    edits.reverse();
    edits
}

fn context_rows(lines: &[DiffLine]) -> Vec<DiffRow> {
    let mut visible = vec![false; lines.len()];
    for (index, line) in lines.iter().enumerate() {
        if line.kind != DiffKind::Same {
            let from = index.saturating_sub(CONTEXT);
            let through = (index + CONTEXT + 1).min(lines.len());
            visible[from..through].fill(true);
        }
    }
    let mut rows = Vec::new();
    let mut gap = 0;
    for (index, visible) in visible.into_iter().enumerate() {
        if visible {
            if gap > 0 {
                rows.push(DiffRow::Gap(std::mem::take(&mut gap)));
            }
            rows.push(DiffRow::Line(index));
        } else {
            gap += 1;
        }
    }
    if gap > 0 {
        rows.push(DiffRow::Gap(gap));
    }
    rows
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_comparison_shortest_edit_paths_are_valid_with_repeated_lines() {
        let samples: Vec<Vec<_>> = (0..=5)
            .flat_map(|len| {
                (0..1 << len).map(move |pattern| (0..len).map(|bit| (pattern >> bit) & 1).collect())
            })
            .collect();
        let cancel = Cancellation::default();
        for left in &samples {
            for right in &samples {
                let (edits, grouped) = edit_script(left, right, &cancel).unwrap();
                assert!(!grouped);
                let mut a = 0;
                let mut b = 0;
                let mut unchanged = 0;
                for kind in edits {
                    match kind {
                        DiffKind::Same => {
                            assert_eq!(left[a], right[b]);
                            a += 1;
                            b += 1;
                            unchanged += 1;
                        }
                        DiffKind::Removed => a += 1,
                        DiffKind::Added => b += 1,
                    }
                }
                assert_eq!((a, b), (left.len(), right.len()));
                assert_eq!(
                    unchanged,
                    lcs_length(left, right),
                    "{left:?} versus {right:?}"
                );
            }
        }
    }

    fn lcs_length(left: &[usize], right: &[usize]) -> usize {
        let mut lengths = vec![vec![0; right.len() + 1]; left.len() + 1];
        for (i, a) in left.iter().enumerate() {
            for (j, b) in right.iter().enumerate() {
                lengths[i + 1][j + 1] = if a == b {
                    lengths[i][j] + 1
                } else {
                    lengths[i][j + 1].max(lengths[i + 1][j])
                };
            }
        }
        lengths[left.len()][right.len()]
    }

    #[test]
    fn file_comparison_bounds_text_work_and_keeps_context() {
        let cancel = Cancellation::default();
        let dense = compare(&"left\n".repeat(800), &"right\n".repeat(800), &cancel)
            .unwrap()
            .unwrap();
        assert!(dense.grouped);
        assert_eq!(
            (dense.removed, dense.added, dense.lines.len()),
            (800, 800, 1600)
        );
        assert!(compare(&"line\n".repeat(MAX_LINES + 1), "", &cancel)
            .unwrap()
            .is_none());

        let before = format!("{}old\n{}", "head\n".repeat(8), "tail\n".repeat(8));
        let after = before.replace("old", "new");
        let context = compare(&before, &after, &cancel).unwrap().unwrap().context;
        assert_eq!(context.first(), Some(&DiffRow::Gap(5)));
        assert_eq!(context.last(), Some(&DiffRow::Gap(5)));
        assert_eq!(context.len(), 10);
    }
}
