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
    if !p.is_dir() {
        DirBuilder::new().recursive(true).mode(0o700).create(&p)?;
    }
    Ok(p)
}

pub fn user_home() -> Option<PathBuf> {
    std::path::absolute(env::var_os("HOME")?).ok()
}
pub fn user_config() -> Option<PathBuf> {
    Some(user_home()?.join(".config/saqta"))
}
pub fn protected_configs() -> Vec<PathBuf> {
    let mut configs: Vec<_> = user_config().into_iter().collect();
    if let Some(p) = env::var_os("XDG_CONFIG_HOME").and_then(|p| std::path::absolute(p).ok()) {
        configs.push(p.join("saqta"));
    }
    configs.extend(
        configs
            .clone()
            .into_iter()
            .filter_map(|p| std::fs::canonicalize(p.join("config.toml")).ok()),
    );
    configs.extend(configs.clone().into_iter().filter_map(|p| {
        std::fs::canonicalize(&p).ok().or_else(|| {
            Some(
                std::fs::canonicalize(p.parent()?)
                    .ok()?
                    .join(p.file_name()?),
            )
        })
    }));
    configs
}
