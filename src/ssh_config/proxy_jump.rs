use super::{
    expand_tokens, host_word, invalid_value, parse_options, resolve_options, ResolvedHost,
};
use crate::vfs::JumpHost;
use std::{net::Ipv6Addr, path::Path};

pub(super) enum Proxy {
    Jump(String, usize),
    Command { disabled: bool },
}

pub(super) fn resolve(
    config: &str,
    home: &Path,
    local_user: &str,
    mut target: ResolvedHost,
    proxy: Option<Proxy>,
) -> Result<ResolvedHost, String> {
    let Some(Proxy::Jump(value, line)) = proxy else {
        return Ok(target);
    };
    if value.eq_ignore_ascii_case("none") {
        return Ok(target);
    }
    let port_text = target.port.to_string();
    let value = expand_tokens(&value, |code| match code {
        'h' => Some(target.host.as_str()),
        'n' => Some(target.alias.as_str()),
        'p' => Some(port_text.as_str()),
        'r' => Some(target.user.as_str()),
        _ => None,
    })
    .ok_or_else(|| invalid_value("ProxyJump", line))?;
    let chain_error = || {
        crate::tr_format!(
            "ProxyJump on line {line}: one directly reachable jump host is supported.",
            line = line
        )
    };
    if value.contains(',') {
        return Err(chain_error());
    }
    let spec = JumpSpec::parse(&value).ok_or_else(|| invalid_value("ProxyJump", line))?;
    let (mut options, matched) = parse_options(config, &spec.alias)?;
    match &options.proxy {
        Some(Proxy::Jump(value, _)) if !value.eq_ignore_ascii_case("none") => {
            return Err(chain_error())
        }
        Some(Proxy::Command { disabled: false }) => return Err(chain_error()),
        _ => {}
    }
    if let Some(user) = spec.user {
        options.user = Some((user, line));
    }
    if let Some(port) = spec.port {
        options.port = Some(port);
    }
    let jump = resolve_options(options, matched, &spec.alias, home, local_user)?;
    if jump.user.is_empty() {
        return Err(invalid_value("ProxyJump", line));
    }
    target.jump = Some(Box::new(JumpHost {
        host: jump.host,
        port: jump.port,
        user: jump.user,
        key: jump.identities.into_iter().next(),
    }));
    for warning in jump.unsupported {
        if !target.unsupported.contains(&warning) {
            target.unsupported.push(warning);
        }
    }
    Ok(target)
}

struct JumpSpec {
    alias: String,
    user: Option<String>,
    port: Option<u16>,
}

impl JumpSpec {
    fn parse(value: &str) -> Option<Self> {
        let (host, user, port) = if value.starts_with("ssh://") {
            let url = url::Url::parse(value).ok()?;
            if url.password().is_some()
                || !url.path().is_empty()
                || url.query().is_some()
                || url.fragment().is_some()
            {
                return None;
            }
            (
                url.host_str()?.to_owned(),
                (!url.username().is_empty()).then(|| url.username().to_owned()),
                url.port(),
            )
        } else {
            let (user, address) = value
                .rsplit_once('@')
                .map_or((None, value), |(user, host)| (Some(user.to_owned()), host));
            let (host, suffix) = if let Some(bracketed) = address.strip_prefix('[') {
                let (host, suffix) = bracketed.split_once(']')?;
                host.parse::<Ipv6Addr>().ok()?;
                (host, suffix)
            } else {
                address
                    .find(':')
                    .map_or((address, ""), |index| address.split_at(index))
            };
            let port = if suffix.is_empty() {
                None
            } else {
                Some(suffix.strip_prefix(':')?.parse::<u16>().ok()?)
            };
            (host.to_owned(), user, port)
        };
        let alias = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(&host);
        if !host_word(alias)
            || alias.contains(['[', ']', '%', '\\'])
            || (alias.contains(':') && alias.parse::<Ipv6Addr>().is_err())
            || port == Some(0)
            || user.as_ref().is_some_and(|user| {
                user.is_empty()
                    || !user
                        .chars()
                        .all(|ch| ch.is_alphanumeric() || "-_.+".contains(ch))
            })
        {
            return None;
        }
        Some(Self {
            alias: alias.to_owned(),
            user,
            port,
        })
    }
}
