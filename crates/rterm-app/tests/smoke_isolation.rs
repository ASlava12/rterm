//! `--smoke` must be a hermetic headless run: it drives a PTY + parser
//! only, and must NOT execute the user's `init.lua` / plugins (which
//! would make the CI check non-deterministic and could have side
//! effects). Runs the built binary as a subprocess with a temp config
//! dir whose `init.lua` writes a sentinel file, then asserts the
//! sentinel was never created.

use std::path::PathBuf;
use std::process::Command;

#[test]
fn smoke_does_not_execute_user_init_lua() {
    // Unique temp dir so parallel test runs don't collide.
    let dir = std::env::temp_dir().join(format!(
        "rterm-smoke-isolation-{}-{}",
        std::process::id(),
        // Cheap extra entropy without pulling in a rng.
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0),
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let sentinel = dir.join("SENTINEL");
    // A config dir with an init.lua that writes the sentinel when run.
    let init_lua = format!(
        "local f = io.open([[{}]], 'w'); if f then f:write('x'); f:close() end\n",
        sentinel.display()
    );
    std::fs::write(dir.join("init.lua"), init_lua).unwrap();
    std::fs::write(dir.join("config.toml"), "[font]\nsize = 13.0\n").unwrap();

    let exe = PathBuf::from(env!("CARGO_BIN_EXE_rterm"));
    let out = Command::new(exe)
        .arg("--config")
        .arg(dir.join("config.toml"))
        .arg("--smoke")
        .output()
        .expect("run rterm --smoke");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("rterm headless OK"),
        "smoke should still succeed; stdout: {stdout}"
    );
    assert!(
        !sentinel.exists(),
        "init.lua must NOT run under --smoke (sentinel was created)"
    );

    std::fs::remove_dir_all(&dir).ok();
}
