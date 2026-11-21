use super::err;
use super::location::Location;
use super::sftp::remote;
use std::io::Read;

pub fn preview_data(
    location: &Location,
    limit: usize,
) -> Result<(ssh2::FileStat, Vec<u8>), String> {
    let (stat, input) = preview_input(location)?;
    let mut bytes = Vec::new();
    if let Some(input) = input {
        input
            .take(limit as u64 + 1)
            .read_to_end(&mut bytes)
            .map_err(err)?;
    }
    Ok((stat, bytes))
}

pub(crate) fn open_preview_stream(location: &Location) -> Result<ssh2::File, String> {
    preview_input(location)?
        .1
        .ok_or_else(|| crate::i18n::text!("Файл изменился перед чтением").into())
}

fn preview_input(location: &Location) -> Result<(ssh2::FileStat, Option<ssh2::File>), String> {
    let sftp = remote(location)?;
    let stat = sftp.lstat(location.path()).map_err(err)?;
    if stat.is_dir() {
        return Ok((stat, None));
    }
    if !stat.is_file() {
        return Err(
            crate::i18n::text!("Предпросмотр ссылок и специальных файлов SFTP недоступен").into(),
        );
    }
    let mut input = sftp.open(location.path()).map_err(err)?;
    let opened = input.stat().map_err(err)?;
    if !opened.is_file() {
        return Err(crate::i18n::text!("Файл изменился перед чтением").into());
    }
    Ok((opened, Some(input)))
}

#[cfg(target_os = "macos")]
pub(crate) fn cache_media_preview(
    location: &Location,
    target: &mut impl std::io::Write,
    limit: u64,
    cancel: &crate::operations::Cancellation,
) -> Result<(), String> {
    let too_large = || {
        crate::i18n::text!("Медиафайл SFTP больше 128 МиБ. Откройте локальную копию.").to_owned()
    };
    cancel.check().map_err(err)?;
    let (stat, input) = preview_input(location)?;
    if stat.size.is_some_and(|size| size > limit) {
        return Err(too_large());
    }
    let mut input = input
        .ok_or_else(|| crate::i18n::text!("Файл изменился перед чтением").to_owned())?
        .take(limit + 1);
    let mut copied = 0;
    let mut buffer = [0; 64 * 1024];
    loop {
        cancel.check().map_err(err)?;
        let count = input.read(&mut buffer).map_err(err)?;
        if count == 0 {
            break;
        }
        copied += count as u64;
        if copied > limit {
            return Err(too_large());
        }
        cancel.check().map_err(err)?;
        target.write_all(&buffer[..count]).map_err(err)?;
    }
    if stat.size.is_some_and(|size| size != copied) {
        return Err(crate::i18n::text!("Файл изменился перед чтением").into());
    }
    cancel.check().map_err(err)?;
    let current = input.get_mut().stat().map_err(err)?;
    if current.size != stat.size || current.mtime != stat.mtime || !current.is_file() {
        return Err(crate::i18n::text!("Файл изменился перед чтением").into());
    }
    target.flush().map_err(err)
}
