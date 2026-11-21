use crate::i18n::text;
use crate::vfs::Location;
use eframe::egui;
use icu_normalizer::ComposingNormalizerBorrowed;
use serde::{Deserialize, Serialize, Serializer};
use std::cmp::Reverse;

const HISTORY_LIMIT: usize = 128;
const MAX_COMPACT_MATCH_STEPS: usize = 8192;

pub struct FolderJump {
    pub previous_focus: Option<egui::Id>,
    query: String,
    highlighted: Option<Location>,
    focus_query: bool,
}

pub enum JumpOutcome {
    Open,
    Dismissed,
    Navigate(Location),
}

impl FolderJump {
    pub fn new(previous_focus: Option<egui::Id>) -> Self {
        Self {
            previous_focus,
            query: String::new(),
            highlighted: None,
            focus_query: true,
        }
    }

    pub fn refocus_query(&mut self) {
        self.focus_query = true;
    }

    // The app routes the same modal navigation keys as the command palette.
    pub fn show(
        &mut self,
        ctx: &egui::Context,
        history: &FolderHistory,
        keys: &[egui::Key],
    ) -> JumpOutcome {
        let modal = egui::Modal::new(egui::Id::unique("folder_jump")).show(ctx, |ui| {
            ui.set_width((ctx.content_rect().width() - 40.0).clamp(160.0, 540.0));
            ui.heading(text("Быстрый переход к папке"));
            let query_id = ui.make_persistent_id("query");
            let moving = keys
                .iter()
                .any(|key| matches!(key, egui::Key::ArrowUp | egui::Key::ArrowDown));
            if (self.focus_query || moving) && !ui.is_sizing_pass() {
                ui.memory_mut(|memory| memory.request_focus(query_id));
                self.focus_query = false;
            }
            let hint = text("Имя или путь папки…");
            let query = ui.add(
                egui::TextEdit::singleline(&mut self.query)
                    .id(query_id)
                    .hint_text(hint)
                    .desired_width(ui.available_width())
                    .lock_focus(true)
                    .return_key(None),
            );
            ctx.accesskit_node_builder(query.id, |node| node.set_label(hint));

            let folders = history.ranked(&self.query);
            let old_highlight = self.highlighted.take();
            let mut index = if query.changed() {
                None
            } else {
                folders
                    .iter()
                    .position(|&location| Some(location) == old_highlight.as_ref())
            }
            .or_else(|| (!folders.is_empty()).then_some(0));
            for &key in keys {
                if key == egui::Key::Escape {
                    return JumpOutcome::Dismissed;
                }
                let Some(current) = index else { continue };
                match key {
                    egui::Key::ArrowUp => {
                        index = Some((current + folders.len() - 1) % folders.len());
                    }
                    egui::Key::ArrowDown => index = Some((current + 1) % folders.len()),
                    egui::Key::Enter => return JumpOutcome::Navigate(folders[current].clone()),
                    _ => {}
                }
            }
            self.highlighted = index.map(|index| folders[index].clone());

            if folders.is_empty() {
                ui.label(text(if self.query.trim().is_empty() {
                    "Недавних папок пока нет"
                } else {
                    "Папки не найдены"
                }));
                return JumpOutcome::Open;
            }

            ui.small(text(if self.query.trim().is_empty() {
                "Порядок зависит от давности и частоты посещений"
            } else {
                "Сначала лучшие совпадения, затем давность и частота посещений"
            }));
            egui::ScrollArea::vertical()
                .max_height((ctx.content_rect().height() - 160.0).clamp(60.0, 340.0))
                .show(ui, |ui| {
                    for location in folders {
                        let path = location.display();
                        let name = location.name();
                        let label = if name == path {
                            path
                        } else {
                            format!("{name}\n{path}")
                        };
                        let selected = self.highlighted.as_ref() == Some(location);
                        let response = ui
                            .push_id(location, |ui| {
                                ui.add(
                                    egui::Button::new(label)
                                        .wrap()
                                        .selected(selected)
                                        .min_size(egui::vec2(ui.available_width(), 30.0)),
                                )
                            })
                            .inner;
                        ctx.accesskit_node_builder(response.id, |node| node.set_selected(selected));
                        if selected && old_highlight.as_ref() != Some(location) {
                            response.scroll_to_me(Some(egui::Align::Center));
                        }
                        if response.gained_focus() && !moving {
                            self.highlighted = Some(location.clone());
                        }
                        if response.clicked() {
                            return JumpOutcome::Navigate(location.clone());
                        }
                    }
                    JumpOutcome::Open
                })
                .inner
        });
        if modal.backdrop_response.clicked() {
            JumpOutcome::Dismissed
        } else {
            modal.inner
        }
    }
}

#[derive(Default, Deserialize, Serialize)]
pub struct FolderHistory {
    // Most recently opened first.
    #[serde(serialize_with = "serialize_visits")]
    visits: Vec<FolderVisit>,
}

#[derive(Deserialize, Serialize)]
struct FolderVisit {
    location: Location,
    count: u32,
}

impl FolderHistory {
    pub fn record_if_new(&mut self, location: &Location) {
        if !self.visits.iter().any(|visit| &visit.location == location) {
            self.record(location);
        }
    }

    pub fn record(&mut self, location: &Location) {
        let count = self
            .visits
            .iter()
            .position(|visit| &visit.location == location)
            .map_or(1, |index| self.visits.remove(index).count.saturating_add(1));
        self.visits.insert(
            0,
            FolderVisit {
                location: location.clone(),
                count,
            },
        );
        self.visits.truncate(HISTORY_LIMIT);
    }

    pub fn ranked(&self, query: &str) -> Vec<&Location> {
        let query = folded(query.trim());
        let mut matches: Vec<_> = self
            .visits
            .iter()
            .take(HISTORY_LIMIT)
            .enumerate()
            .filter_map(|(age, visit)| {
                let quality = if query.is_empty() {
                    (0, 0)
                } else {
                    match_quality(&query, &visit.location)?
                };
                // Each doubling of visits offsets up to 16 newer folders.
                // Cap the boost so old favorites can be displaced by current work.
                let frequency = visit.count.max(1).ilog2().min(5) as usize * 16;
                let recency = HISTORY_LIMIT - age;
                Some((
                    (quality, Reverse(recency + frequency), age),
                    &visit.location,
                ))
            })
            .collect();
        matches.sort_unstable_by_key(|(key, _)| *key);
        matches.into_iter().map(|(_, location)| location).collect()
    }
}

// PathBuf's serde representation requires UTF-8. Keep these visits usable
// during the session without preventing the rest of the settings from saving.
fn serialize_visits<S>(visits: &[FolderVisit], serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    serializer.collect_seq(
        visits
            .iter()
            .take(HISTORY_LIMIT)
            .filter(|visit| visit.location.has_utf8_paths()),
    )
}

fn folded(text: &str) -> String {
    let lowercase: String = text.chars().flat_map(char::to_lowercase).collect();
    ComposingNormalizerBorrowed::new_nfc()
        .normalize(&lowercase)
        .into_owned()
}

fn match_quality(query: &str, location: &Location) -> Option<(u8, usize)> {
    let name = folded(&location.name());
    let path = folded(&location.path().to_string_lossy());
    let address = location.remote().then(|| folded(&location.display()));
    let mut paths = std::iter::once(path.as_str()).chain(address.as_deref());

    if name == query || paths.clone().any(|path| path == query) {
        return Some((0, 0));
    }
    if name.starts_with(query) {
        return Some((1, 0));
    }
    if name.contains(query) {
        return Some((2, 0));
    }
    if paths.clone().any(|path| path.starts_with(query)) {
        return Some((3, 0));
    }
    if paths.any(|path| path.contains(query)) {
        return Some((4, 0));
    }
    if let Some(gaps) = subsequence_gaps(query, &name) {
        return Some((5, gaps));
    }
    std::iter::once(path.as_str())
        .chain(address.as_deref())
        .filter_map(|path| subsequence_gaps(query, path))
        .min()
        .map(|gaps| (6, gaps))
}

fn subsequence_gaps(query: &str, text: &str) -> Option<usize> {
    if query.is_empty() || query.len() > text.len() {
        return None;
    }
    let wanted: Vec<_> = query.chars().collect();
    let text_len = text.chars().count();
    if wanted.len() > text_len {
        return None;
    }
    let mut best = first_subsequence_gaps(&wanted, text)?;
    // Every character can extend only this many feasible prefixes. Keep
    // fuzzy matching responsive for long pasted queries and long paths.
    let max_prefixes = wanted.len().min(text_len - wanted.len() + 1);
    if best == 0 || max_prefixes > MAX_COMPACT_MATCH_STEPS / text_len {
        return Some(best);
    }
    let mut latest_start = vec![None; wanted.len()];
    for (index, ch) in text.chars().enumerate() {
        // For each query prefix, keep its latest start. Updating backwards
        // lets this character extend only prefixes matched before it.
        // Skip prefixes that cannot finish in the remaining characters.
        let first = wanted.len().saturating_sub(text_len - index);
        let end = wanted.len().min(index + 1);
        for prefix in (first..end).rev() {
            if ch != wanted[prefix] {
                continue;
            }
            let start = if prefix == 0 {
                Some(index)
            } else {
                latest_start[prefix - 1]
            };
            if let Some(start) = start {
                latest_start[prefix] = Some(start);
                if prefix == wanted.len() - 1 {
                    let gaps = index - start - prefix;
                    if gaps == 0 {
                        return Some(0);
                    }
                    best = best.min(gaps);
                }
            }
        }
    }
    Some(best)
}

fn first_subsequence_gaps(wanted: &[char], text: &str) -> Option<usize> {
    let mut prefix = 0;
    let mut start = 0;
    for (index, ch) in text.chars().enumerate() {
        if ch == wanted[prefix] {
            if prefix == 0 {
                start = index;
            }
            prefix += 1;
            if prefix == wanted.len() {
                return Some(index - start + 1 - prefix);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use std::path::PathBuf;

    fn folder(name: &str) -> Location {
        PathBuf::from("workspace").join(name).into()
    }

    #[test]
    fn exact_and_literal_names_beat_fuzzy_matches_regardless_of_visit_count() {
        let exact = folder("pc");
        let prefix = folder("PC build");
        let substring = folder("temp-pc");
        let fuzzy = folder("PawCommander");
        let mut history = FolderHistory::default();
        for location in [&exact, &prefix, &substring] {
            history.record(location);
        }
        for _ in 0..64 {
            history.record(&fuzzy);
        }
        assert_eq!(
            history.ranked(" PC "),
            vec![&exact, &prefix, &substring, &fuzzy]
        );
        assert!(history.ranked("zzzz").is_empty());
    }

    #[test]
    fn fuzzy_matches_use_the_tightest_span_instead_of_the_first() {
        let tight_later = PathBuf::from("a____x_a_c").into();
        let wide = PathBuf::from("a___c").into();
        let mut history = FolderHistory::default();
        history.record(&tight_later);
        history.record(&wide);
        assert_eq!(history.ranked("ac"), vec![&tight_later, &wide]);
        assert_eq!(subsequence_gaps("аав", "ав_а_а_в"), Some(2));
        assert_eq!(subsequence_gaps("aab", "ab"), None);
        let repeated = "a".repeat(2048);
        assert_eq!(
            subsequence_gaps(&format!("{repeated}z"), &format!("{repeated}_z")),
            Some(1)
        );
        assert_eq!(
            subsequence_gaps(&format!("{repeated}z"), &format!("{repeated}{repeated}z")),
            Some(repeated.len())
        );
    }

    #[test]
    fn repeated_visits_raise_rank_and_stale_history_is_bounded() {
        let frequent = folder("frequent");
        let newest = folder("newest");
        let mut history = FolderHistory::default();
        history.record(&frequent);
        history.record(&newest);
        assert_eq!(history.ranked(""), vec![&newest, &frequent]);
        history.record(&frequent);
        history.record(&newest);
        history.record(&frequent);
        history.record(&frequent);
        history.record(&newest);
        assert_eq!(history.ranked(""), vec![&frequent, &newest]);
        for index in 0..HISTORY_LIMIT {
            history.record(&folder(&format!("folder-{index}")));
        }
        assert_eq!(history.ranked("").len(), HISTORY_LIMIT);
        assert!(history.ranked("frequent").is_empty());
    }

    #[test]
    fn history_round_trips_distinct_backends_and_matches_unicode_paths() {
        let local = folder("Café/Документы");
        let remote = Location::Sftp {
            endpoint: Endpoint {
                host: "test.example".into(),
                port: 2222,
                user: "tester".into(),
                key: None,
                jump: None,
            },
            path: PathBuf::from("/Café/Документы"),
        };
        let mut history = FolderHistory::default();
        history.record(&local);
        history.record(&remote);
        history.record(&local);
        let persisted = serde_json::to_string(&history).unwrap();
        let restored: FolderHistory = serde_json::from_str(&persisted).unwrap();
        assert_eq!(restored.ranked("Cafe\u{301}"), vec![&local, &remote]);
        assert_eq!(restored.ranked("дкм"), vec![&local, &remote]);
        assert_eq!(restored.ranked("test.example:2222"), vec![&remote]);
    }

    #[cfg(unix)]
    #[test]
    fn a_non_utf8_folder_stays_searchable_without_breaking_settings() {
        use std::os::unix::ffi::OsStringExt;

        let non_utf8 = Location::Local(PathBuf::from(std::ffi::OsString::from_vec(vec![
            b'a', b'b', 0xff,
        ])));
        let mut history = FolderHistory::default();
        history.record(&non_utf8);
        assert_eq!(history.ranked("ab"), vec![&non_utf8]);
        let persisted = serde_json::to_string(&history).unwrap();
        let restored: FolderHistory = serde_json::from_str(&persisted).unwrap();
        assert!(restored.ranked("").is_empty());
    }
}
