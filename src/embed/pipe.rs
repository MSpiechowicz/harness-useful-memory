//! What keeps the embedder's Windows named pipe to this user.
//!
//! A named pipe's name is visible to every user and the first one to create it owns it, so the
//! pipe is protected three ways:
//!
//! - it is created with a security descriptor that lets only the user running the embedder (and
//!   SYSTEM) open it, so no other user can read what is sent or hold its connections;
//! - its name carries a secret that is new at every start of the embedder and kept, with the
//!   embedder's process id, in [`STATE_FILE`] of the data folder (a folder of this user), so no
//!   other user can create the pipe ahead of the embedder;
//! - a client checks, after it connected, that the process serving the pipe is the one recorded
//!   there, so it never talks to a process that took over the name.

use std::io::{Error, ErrorKind, Result};
use std::path::Path;

use interprocess::local_socket::Stream;
use interprocess::local_socket::traits::StreamCommon as _;
use interprocess::os::windows::security_descriptor::SecurityDescriptor;
use widestring::U16CString;

use crate::support::private::{read_private, write_atomic};

/// The file of the data folder that holds the process id and the pipe secret of the embedder.
pub const STATE_FILE: &str = "embedder.pipe";

/// The security descriptor of the pipe: a protected DACL (nothing inherited) that gives full
/// access to the owner of the pipe, which is the user running the embedder, and to SYSTEM.
const ONLY_THIS_USER: &str = "D:P(A;;GA;;;OW)(A;;GA;;;SY)";

/// What the running embedder recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Published {
    pub pid: u32,
    pub secret: String,
}

/// Records a new secret and this process's id, replacing what an earlier embedder left.
pub fn publish(root: &Path) -> Result<()> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(Error::other)?;

    let secret: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    let text = format!("{} {secret}\n", std::process::id());

    write_atomic(
        root,
        &root.join(STATE_FILE),
        text.as_bytes(),
        "embedder-pipe-",
        "pipe state",
    )
    .map_err(Error::other)
}

/// What the embedder of `root` recorded. A missing file is `NotFound`: no embedder started.
pub fn read(root: &Path) -> Result<Published> {
    let bytes = read_private(&root.join(STATE_FILE), 128)
        .map_err(|message| Error::new(ErrorKind::PermissionDenied, message))?
        .ok_or_else(|| Error::new(ErrorKind::NotFound, "no embedder has started"))?;

    parse(&String::from_utf8_lossy(&bytes))
        .ok_or_else(|| Error::new(ErrorKind::InvalidData, "an unreadable pipe state"))
}

fn parse(text: &str) -> Option<Published> {
    let (pid, secret) = text.trim().split_once(' ')?;

    if secret.len() != 32 || !secret.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }

    Some(Published {
        pid: pid.parse().ok()?,
        secret: secret.to_owned(),
    })
}

/// Forgets what [`publish`] recorded (the embedder is stopping).
pub fn withdraw(root: &Path) {
    let _ = std::fs::remove_file(root.join(STATE_FILE));
}

/// The descriptor the pipe is created with.
pub fn only_this_user() -> Result<SecurityDescriptor> {
    let sddl = U16CString::from_str(ONLY_THIS_USER).map_err(|error| Error::new(ErrorKind::InvalidInput, error))?;
    SecurityDescriptor::deserialize(&sddl)
}

/// Fails unless the process serving `stream` is the embedder `published` names.
pub fn check_server(stream: &Stream, published: &Published) -> Result<()> {
    if stream.peer_creds()?.pid() == Some(published.pid) {
        return Ok(());
    }

    Err(Error::new(
        ErrorKind::PermissionDenied,
        "the pipe is served by a process that is not this user's embedder",
    ))
}
