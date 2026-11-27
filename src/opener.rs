//! Opens links and folders with the desktop, through the `open` crate.

use std::ffi::OsStr;
use std::io;

/// Opens a URL or a path in whatever the desktop uses for it.
#[cfg(not(test))]
pub fn open(target: impl AsRef<OsStr>) -> io::Result<()> {
    open::that(target.as_ref())
}

#[cfg(test)]
pub fn open(_target: impl AsRef<OsStr>) -> io::Result<()> {
    Ok(())
}
