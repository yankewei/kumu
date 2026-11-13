//! One process and the cells its TUI has drawn.
//!
//! Used for both an agent's interactive TUI and a plain login shell. `pty`
//! starts the process. `vt` turns the byte stream into a grid and answers
//! terminal queries. `session` owns both on one thread so a query reply is
//! written back before the next read.

mod pty;
mod session;
mod vt;

pub use session::Term;
pub use vt::{
    Cell, CursorShape, KeyInput, MouseAction, MouseButton, MouseInput, MouseMods, Rgb, SelectStep,
    Snapshot, TermColors, TermNotification,
};

/// The user's login shell: `$SHELL`, falling back to `/bin/sh`.
/// Load login profiles so Finder-launched terminals get the user's tool paths.
pub fn default_shell() -> (String, Vec<String>) {
    let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    (program, vec!["-l".into()])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "macos")]
    #[test]
    fn shell_startup_loads_login_environment() {
        let home = std::env::temp_dir().join(format!("kumu-login-shell-{}", std::process::id()));
        std::fs::create_dir(&home).expect("create login shell fixture");
        std::fs::write(
            home.join(".zprofile"),
            "export KUMU_LOGIN_PROFILE_LOADED=1\n",
        )
        .expect("write login profile");

        let (_, args) = default_shell();
        let output = std::process::Command::new("/bin/zsh")
            .args(args)
            .args(["-c", "printf '%s' \"$KUMU_LOGIN_PROFILE_LOADED\""])
            .env_clear()
            .env("HOME", &home)
            .env("ZDOTDIR", &home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .output();
        std::fs::remove_dir_all(&home).expect("remove login shell fixture");

        let output = output.expect("launch zsh with default shell arguments");
        assert!(output.status.success());
        assert_eq!(output.stdout, b"1");
    }
}
