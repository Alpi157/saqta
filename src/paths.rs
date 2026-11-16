use std::{env, fs::DirBuilder, io, os::unix::fs::DirBuilderExt, path::PathBuf};
pub fn home() -> io::Result<PathBuf> {
    let p = if let Some(p) = env::var_os("SAQTA_HOME") {
        PathBuf::from(p)
    } else if let Some(p) = env::var_os("XDG_DATA_HOME") {
        PathBuf::from(p).join("saqta")
    } else {
        PathBuf::from(env::var_os("HOME").ok_or_else(|| io::Error::other("HOME is unset"))?)
            .join(".local/share/saqta")
    };
    let p = std::path::absolute(p)?;
    // The creation mode applies only to new directories. Never chmod an existing home.
    DirBuilder::new().recursive(true).mode(0o700).create(&p)?;
    Ok(p)
}
