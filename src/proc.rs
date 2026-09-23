//! Subprocess spawning for a windowless daemon.
//!
//! The server runs without a console. On Windows a console program (`git`,
//! `cargo`) started from such a process gets a fresh console window of its
//! own unless it is created with `CREATE_NO_WINDOW` — one visible flash per
//! call. Every subprocess this crate starts goes through [`hidden`].

use std::process::Command;

/// A [`Command`] for `program` that never opens a console window.
pub(crate) fn hidden(program: &str) -> Command {
    #[allow(unused_mut)]
    let mut cmd = Command::new(program);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd
}
