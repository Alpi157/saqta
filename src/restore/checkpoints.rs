//! Write plans before mutations; replace the compact checkpoint every 128 records.
use super::*;

pub(super) struct Checkpoints {
    folder: PathBuf,
    log: File,
    count: usize,
    since: Instant,
}

pub(super) fn load(folder: &Path) -> io::Result<Recovery> {
    let mut recovery =
        serde_json::from_slice(&fs::read(folder.join("state.json"))?).map_err(io::Error::other)?;
    let bytes = match fs::read(folder.join("state.log")) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(recovery),
        Err(e) => return Err(e),
    };
    // An incomplete final write cannot have authorized its mutation. Retain it
    // on open; malformed complete frames are damage and must refuse recovery.
    if let Some(end) = bytes.iter().rposition(|&b| b == b'\n') {
        for line in bytes[..end].split(|&b| b == b'\n') {
            recovery = serde_json::from_slice(line).map_err(io::Error::other)?;
        }
    }
    Ok(recovery)
}

impl Checkpoints {
    pub(super) fn open(folder: &Path, recovery: &Recovery) -> io::Result<Self> {
        let path = folder.join("state.log");
        if let Ok(bytes) = fs::read(&path)
            && !bytes.is_empty()
            && !bytes.ends_with(b"\n")
        {
            journal::json_write(&folder.join("state.json"), recovery)?;
            fs::rename(
                &path,
                folder.join(format!("state-tail-{}.log", journal::now())),
            )?;
        }
        let log = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(path)?;
        Ok(Self {
            folder: folder.to_owned(),
            log,
            count: 128,
            since: Instant::now(),
        })
    }
    pub(super) fn record(&mut self, recovery: &Recovery) -> io::Result<()> {
        let mut frame = serde_json::to_vec(recovery).map_err(io::Error::other)?;
        frame.push(b'\n');
        self.log.write_all(&frame)?;
        self.count += 1;
        if self.count >= 128 || self.since.elapsed().as_millis() >= 250 {
            self.publish(recovery)?;
        }
        Ok(())
    }
    pub(super) fn publish(&mut self, recovery: &Recovery) -> io::Result<()> {
        journal::json_write(&self.folder.join("state.json"), recovery)?;
        // Publication precedes truncation. A crash sees either the old log's
        // matching last frame, or the new checkpoint and an empty/partial log.
        self.log.set_len(0)?;
        self.count = 0;
        self.since = Instant::now();
        Ok(())
    }
}
