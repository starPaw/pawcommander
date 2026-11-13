use super::err;
use super::location::Location;
use super::sftp::remote;
use std::io::Read;

pub fn preview_data(
    location: &Location,
    limit: usize,
) -> Result<(ssh2::FileStat, Vec<u8>), String> {
    let sftp = remote(location)?;
    let stat = sftp.lstat(location.path()).map_err(err)?;
    if stat.is_dir() {
        return Ok((stat, Vec::new()));
    }
    if !stat.is_file() {
        return Err("Предпросмотр ссылок и специальных файлов SFTP недоступен".into());
    }
    let mut input = sftp.open(location.path()).map_err(err)?;
    let opened = input.stat().map_err(err)?;
    if !opened.is_file() {
        return Err("Файл изменился перед чтением".into());
    }
    let mut bytes = Vec::new();
    input
        .take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(err)?;
    Ok((opened, bytes))
}
