//! Resolve connection form values from the user's OpenSSH config.

use crate::{i18n::text, name_match::Mask};
use std::{
    collections::HashSet,
    fs::OpenOptions,
    io::{self, Read},
    path::{Path, PathBuf},
};

#[cfg(test)]
mod tests;

const MAX_CONFIG_BYTES: u64 = 1024 * 1024;
const MAX_OPTION_BYTES: usize = 16 * 1024;

#[derive(Debug)]
pub(crate) struct ResolvedHost {
    pub alias: String,
    pub matched: bool,
    pub host: String,
    pub user: String,
    pub port: u16,
    pub identities: Vec<PathBuf>,
    pub unsupported: Vec<Unsupported>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Unsupported {
    pub keyword: String,
    pub line: usize,
}

pub(crate) fn load_user_config(alias: &str) -> Result<ResolvedHost, String> {
    let home = dirs::home_dir().ok_or_else(|| text("Не найдена домашняя папка"))?;
    let user = std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_default();
    load_config(&home, alias, &user)
}

pub(crate) fn load_config(home: &Path, alias: &str, user: &str) -> Result<ResolvedHost, String> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK);
    }
    let file = options
        .open(home.join(".ssh/config"))
        .map_err(config_read_error)?;
    let metadata = file.metadata().map_err(config_read_error)?;
    if !metadata.is_file() {
        return Err(text("~/.ssh/config должен быть обычным файлом").into());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // SAFETY: geteuid has no arguments or preconditions.
        let owner = unsafe { libc::geteuid() };
        if (metadata.uid() != owner && metadata.uid() != 0) || metadata.mode() & 0o022 != 0 {
            return Err(text("Проверьте владельца и права записи ~/.ssh/config").into());
        }
    }
    let mut config = String::new();
    file.take(MAX_CONFIG_BYTES + 1)
        .read_to_string(&mut config)
        .map_err(config_read_error)?;
    if config.len() as u64 > MAX_CONFIG_BYTES {
        return Err(text("~/.ssh/config больше 1 МиБ").into());
    }
    resolve(&config, alias, home, user)
}

fn config_read_error(error: io::Error) -> String {
    crate::tr_format!(
        "Не удалось прочитать ~/.ssh/config ({error})",
        "Could not read ~/.ssh/config ({error})"
    )
}

#[derive(Default)]
struct Options {
    hostname: Option<(String, usize)>,
    user: Option<(String, usize)>,
    port: Option<u16>,
    identities: Vec<(String, usize)>,
    unsupported: Vec<Unsupported>,
    warned: HashSet<String>,
}

impl Options {
    fn warn(&mut self, key: &str, keyword: &str, line: usize) {
        if self.warned.insert(key.to_owned()) {
            self.unsupported.push(Unsupported {
                keyword: keyword.to_owned(),
                line,
            });
        }
    }
}

fn resolve(
    config: &str,
    alias: &str,
    home: &Path,
    local_user: &str,
) -> Result<ResolvedHost, String> {
    let alias = alias.trim();
    if !host_word(alias) {
        return Err(text("Введите имя сервера или SSH-алиас").into());
    }
    let mut options = Options::default();
    let mut active = true;
    let mut matched = false;
    for (index, raw) in config
        .strip_prefix('\u{feff}')
        .unwrap_or(config)
        .lines()
        .enumerate()
    {
        let Some((keyword, rest)) = directive(raw) else {
            continue;
        };
        let line = index + 1;
        if keyword.is_empty()
            || !keyword
                .bytes()
                .all(|ch| ch.is_ascii_alphanumeric() || ch == b'-' || ch == b'_')
        {
            return Err(invalid_value("SSH config", line));
        }
        let key = keyword.to_ascii_lowercase();
        let invalid = || invalid_value(keyword, line);
        match key.as_str() {
            "host" => {
                let patterns = arguments(rest).map_err(|()| invalid())?;
                if patterns.is_empty()
                    || patterns
                        .iter()
                        .any(|pattern| pattern.is_empty() || pattern == "!")
                {
                    return Err(invalid());
                }
                active = host_matches(&patterns, alias);
                matched |= active;
            }
            "match" => {
                options.warn(&key, keyword, line);
                active = false;
            }
            _ if !active => {}
            "hostname" | "user" | "port" | "identityfile" => {
                let [value]: [String; 1] = arguments(rest)
                    .map_err(|()| invalid())?
                    .try_into()
                    .map_err(|_| invalid())?;
                if value.is_empty() || value.chars().any(char::is_control) {
                    return Err(invalid());
                }
                matched = true;
                match key.as_str() {
                    "hostname" => {
                        options.hostname.get_or_insert((value, line));
                    }
                    "user" => {
                        options.user.get_or_insert((value, line));
                    }
                    "port" => {
                        let port = value
                            .parse::<u16>()
                            .ok()
                            .filter(|&port| port != 0)
                            .ok_or_else(invalid)?;
                        options.port.get_or_insert(port);
                    }
                    _ if !value.eq_ignore_ascii_case("none") => {
                        options.identities.push((value, line))
                    }
                    _ => {}
                }
            }
            _ => options.warn(&key, keyword, line),
        }
    }
    let host = match options.hostname {
        Some((value, line)) => expand_tokens(&value, |code| (code == 'h').then_some(alias))
            .filter(|host| host_word(host))
            .ok_or_else(|| invalid_value("HostName", line))?,
        None => alias.to_owned(),
    };
    let port = options.port.unwrap_or(22);
    let port_text = port.to_string();
    let local_token = |code| match code {
        'd' => home.to_str(),
        'h' => Some(host.as_str()),
        'n' => Some(alias),
        'p' => Some(port_text.as_str()),
        'u' => (!local_user.is_empty()).then_some(local_user),
        _ => None,
    };
    let user = match options.user {
        Some((value, line)) => expand_tokens(&value, local_token)
            .filter(|user| {
                !user.is_empty() && !user.chars().any(|ch| ch.is_control() || ch.is_whitespace())
            })
            .ok_or_else(|| invalid_value("User", line))?,
        None => local_user.to_owned(),
    };
    let mut identities = Vec::new();
    let mut seen = HashSet::new();
    for (value, line) in options.identities {
        let path = expand_tokens(&value, |code| match code {
            'r' => (!user.is_empty()).then_some(user.as_str()),
            _ => local_token(code),
        })
        .and_then(|value| expand_environment(&value, home))
        .and_then(|value| identity_path(&value, home))
        .ok_or_else(|| invalid_value("IdentityFile", line))?;
        if seen.insert(path.clone()) {
            identities.push(path);
        }
    }
    Ok(ResolvedHost {
        alias: alias.to_owned(),
        matched,
        host,
        user,
        port,
        identities,
        unsupported: options.unsupported,
    })
}

fn host_word(value: &str) -> bool {
    !value.is_empty()
        && !value
            .chars()
            .any(|c| c.is_control() || c.is_whitespace() || matches!(c, '/' | '@' | '*' | '?'))
}

fn host_matches(patterns: &[String], alias: &str) -> bool {
    let mut matched = false;
    for pattern in patterns {
        let negative = pattern.strip_prefix('!');
        if Mask::new(negative.unwrap_or(pattern)).matches(alias) {
            if negative.is_some() {
                return false;
            }
            matched = true;
        }
    }
    matched
}

fn directive(line: &str) -> Option<(&str, &str)> {
    let line = line.trim_start();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let boundary = line
        .find(|ch: char| ch.is_ascii_whitespace() || matches!(ch, '=' | '#'))
        .unwrap_or(line.len());
    let (keyword, rest) = line.split_at(boundary);
    let rest = rest.trim_start();
    Some((keyword, rest.strip_prefix('=').unwrap_or(rest)))
}

fn arguments(input: &str) -> Result<Vec<String>, ()> {
    let mut args = Vec::new();
    let mut token = String::new();
    let mut quote = None;
    let mut present = false;
    let mut chars = input.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (None, '#') => break,
            (None, ch) if ch.is_ascii_whitespace() => {
                if present {
                    args.push(std::mem::take(&mut token));
                    present = false;
                }
                continue;
            }
            (None, '"' | '\'') => quote = Some(ch),
            (Some(end), ch) if end == ch => quote = None,
            (_, '\\')
                if chars.peek().is_some_and(|next| {
                    matches!(*next, '\\' | '"' | '\'' | '#')
                        || (quote.is_none() && next.is_ascii_whitespace())
                }) =>
            {
                token.push(chars.next().unwrap())
            }
            _ => token.push(ch),
        }
        present = true;
    }
    if quote.is_some() {
        return Err(());
    }
    if present {
        args.push(token);
    }
    Ok(args)
}

fn expand_tokens<'a>(value: &str, lookup: impl Fn(char) -> Option<&'a str>) -> Option<String> {
    if value.len() > MAX_OPTION_BYTES {
        return None;
    }
    let mut output = String::with_capacity(value.len());
    let mut chars = value.chars();
    while let Some(ch) = chars.next() {
        if ch == '%' {
            let code = chars.next()?;
            output.push_str(if code == '%' { "%" } else { lookup(code)? });
        } else {
            output.push(ch);
        }
        if output.len() > MAX_OPTION_BYTES {
            return None;
        }
    }
    Some(output)
}

fn expand_environment(mut value: &str, home: &Path) -> Option<String> {
    let mut output = String::with_capacity(value.len());
    while let Some((before, expression)) = value.split_once("${") {
        let (name, remaining) = expression.split_once('}')?;
        output.push_str(before);
        if name == "HOME" {
            output.push_str(home.to_str()?);
        } else {
            output.push_str(&std::env::var(name).ok()?);
        }
        if output.len() > MAX_OPTION_BYTES {
            return None;
        }
        value = remaining;
    }
    output.push_str(value);
    (output.len() <= MAX_OPTION_BYTES).then_some(output)
}

fn identity_path(value: &str, home: &Path) -> Option<PathBuf> {
    if value.is_empty() || value.chars().any(char::is_control) {
        None
    } else if value == "~" {
        Some(home.to_owned())
    } else if let Some(relative) = value.strip_prefix("~/") {
        Some(home.join(relative))
    } else if value.starts_with('~') {
        None
    } else {
        Some(PathBuf::from(value))
    }
}

fn invalid_value(keyword: &str, line: usize) -> String {
    crate::tr_format!(
        "Неверное или неподдерживаемое значение {keyword} в строке {line}",
        "Invalid or unsupported value for {keyword} on line {line}"
    )
}
