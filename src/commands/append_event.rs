use anyhow::Context;

use crate::{event::Event, formats::rmp::BinaryWriter, lock_osh_dir};

/// # Errors
///
/// Returns an `Err` if no osh file can be created or exists and if writing to it fails.
pub fn invoke(
    starttime: i64,
    command: &str,
    folder: &str,
    endtime: i64,
    exit_code: i16,
    machine: &str,
    session: &str,
) -> anyhow::Result<()> {
    if command.starts_with(' ') {
        return Ok(());
    }

    let mut osh_file = home::home_dir().context("home dir has to exist")?;
    osh_file.push(".osh/");
    std::fs::create_dir_all(&osh_file)?;
    osh_file.push("local.bosh");

    // the file may be replaced by a rewrite (see `remove_command`) between open and write
    let _lock = lock_osh_dir()?;

    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(osh_file.as_path())?;
    let mut writer = BinaryWriter::new(file);
    Event {
        timestamp_millis: starttime,
        command: command.to_string(),
        endtime,
        exit_code,
        folder: folder.to_string(),
        machine: machine.to_string(),
        session: session.to_string(),
    }
    .write(&mut writer)?;
    writer.flush()?;

    Ok(())
}
