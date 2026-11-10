use crate::vfs::{self, Location};
use eframe::egui;
use std::sync::mpsc::{self, Receiver, TryRecvError};

#[derive(Clone)]
pub struct Suggestion {
    pub name: String,
    pub path: Location,
    folded_name: String,
}

#[derive(Default)]
pub struct Completion {
    pub edited: bool,
    pub selected: Option<usize>,
    cached: Option<(Location, Vec<Suggestion>)>,
    pending: Option<(Location, Receiver<Vec<Suggestion>>)>,
}

impl Completion {
    pub fn reset(&mut self) {
        self.edited = false;
        self.selected = None;
        self.cached = None;
    }

    pub fn suggestions(
        &mut self,
        address: &str,
        current: &Location,
        ctx: &egui::Context,
    ) -> Vec<Suggestion> {
        if let Some((parent, receiver)) = &self.pending {
            let result = match receiver.try_recv() {
                Ok(entries) => Some(entries),
                Err(TryRecvError::Disconnected) => Some(vec![]),
                Err(TryRecvError::Empty) => None,
            };
            if let Some(entries) = result {
                if self.edited {
                    self.cached = Some((parent.clone(), entries));
                }
                self.pending = None;
            }
        }
        if !self.edited {
            return vec![];
        }
        let Some((parent, prefix)) = query(address, current) else {
            return vec![];
        };
        if let Some((_, entries)) = self.cached.as_ref().filter(|(path, _)| *path == parent) {
            return entries
                .iter()
                .filter(|item| {
                    item.folded_name.starts_with(&prefix)
                        && (prefix.starts_with('.') || !item.name.starts_with('.'))
                })
                .take(8)
                .cloned()
                .collect();
        }
        if self.pending.is_none() {
            let (sender, receiver) = mpsc::channel();
            let path = parent.clone();
            let ctx = ctx.clone();
            self.pending = Some((parent, receiver));
            let _ = std::thread::Builder::new()
                .name("pawcommander-path-completion".into())
                .spawn(move || {
                    let _ = sender.send(read_folders(&path));
                    ctx.request_repaint();
                });
        }
        vec![]
    }
}

fn query(address: &str, current: &Location) -> Option<(Location, String)> {
    let url = address.starts_with("sftp:");
    if url && !address.strip_prefix("sftp://")?.contains('/') {
        return None;
    }
    let windows_path = cfg!(windows) && !url && !current.remote();
    let split = address
        .rfind(|ch| ch == '/' || (windows_path && ch == '\\'))
        .map_or(0, |index| index + 1);
    let (parent, prefix) = address.split_at(split);
    let parent = vfs::resolve(parent, current).ok()?;
    let prefix = if url {
        percent_encoding::percent_decode_str(prefix)
            .decode_utf8()
            .ok()?
            .to_lowercase()
    } else {
        prefix.to_lowercase()
    };
    Some((parent, prefix))
}

fn read_folders(parent: &Location) -> Vec<Suggestion> {
    let mut folders: Vec<_> = vfs::read_address_directory(parent)
        .unwrap_or_default()
        .into_iter()
        .filter(|entry| entry.is_dir && !entry.name.contains(['\r', '\n']))
        .filter_map(|entry| {
            let name = entry.path.path().file_name()?.to_str()?.to_owned();
            Some(Suggestion {
                folded_name: name.to_lowercase(),
                name,
                path: entry.path,
            })
        })
        .collect();
    folders.sort_unstable_by(|a, b| a.folded_name.cmp(&b.folded_name).then(a.name.cmp(&b.name)));
    folders
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vfs::Endpoint;
    use std::path::PathBuf;

    #[test]
    fn remote_completion_uses_the_decoded_path_and_current_session_key() {
        let current = Location::Sftp {
            endpoint: Endpoint {
                host: "example.invalid".into(),
                port: 2222,
                user: "alice".into(),
                key: Some(PathBuf::from("test-key")),
            },
            path: "/work".into(),
        };
        let (parent, prefix) = query(
            "sftp://alice@example.invalid:2222/Two%20Words/Ne%20",
            &current,
        )
        .unwrap();
        assert_eq!(parent, current.with_path("/Two Words".into()));
        assert_eq!(prefix, "ne ");
        assert!(query("sftp://alice@example.invalid:2222", &current).is_none());
    }
}
