//! Progress output must never interrupt an operation that changes files.
use std::{fmt, io::Write};

pub struct Progress<W> {
    writer: W,
    failed: bool,
}

impl<W: Write> Progress<W> {
    pub fn new(writer: W) -> Self {
        Self {
            writer,
            failed: false,
        }
    }

    pub fn silent(writer: W) -> Self {
        Self {
            writer,
            failed: true,
        }
    }

    pub fn line(&mut self, args: fmt::Arguments<'_>) {
        if !self.failed {
            self.failed = writeln!(self.writer, "{args}")
                .and_then(|()| self.writer.flush())
                .is_err();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io;

    struct FailingWriter {
        writes: usize,
        error: io::ErrorKind,
        fail_flush: bool,
    }
    impl Write for FailingWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.fail_flush {
                Ok(bytes.len())
            } else {
                Err(self.error.into())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(self.error.into())
        }
    }

    #[test]
    fn output_stops_after_write_or_flush_failure() {
        for error in [io::ErrorKind::BrokenPipe, io::ErrorKind::Other] {
            for fail_flush in [false, true] {
                let writer = FailingWriter {
                    writes: 0,
                    error,
                    fail_flush,
                };
                let mut output = Progress::new(writer);
                output.line(format_args!("first"));
                assert!(output.failed);
                let writes = output.writer.writes;
                output.line(format_args!("second"));
                assert_eq!(output.writer.writes, writes);
            }
        }
    }
}
