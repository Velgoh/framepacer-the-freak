use crate::logparse::{LogLine, parse_line};
use std::fs::File;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
use std::path::PathBuf;

pub struct LogTailer {
    path: PathBuf,
    reader: Option<BufReader<File>>,
    last_offset: u64,
}

impl LogTailer {
    /// Creates a tailer for the specified path.
    /// If `start_at_end` is true, begins reading only newly appended lines.
    pub fn new(path: impl Into<PathBuf>, start_at_end: bool) -> Self {
        let path = path.into();
        let mut tailer = Self {
            path,
            reader: None,
            last_offset: 0,
        };
        if start_at_end {
            tailer.seek_to_end();
        }
        tailer
    }

    pub fn seek_to_end(&mut self) {
        if let Ok(metadata) = std::fs::metadata(&self.path) {
            self.last_offset = metadata.len();
            if let Some(reader) = self.reader.as_mut() {
                let _ = reader.seek(SeekFrom::Start(self.last_offset));
            }
        }
    }

    /// Reads all currently available new lines.
    /// Handles file truncation/rotation by seeking back to 0 if file size shrank.
    pub fn poll_new_lines(&mut self) -> Vec<String> {
        let mut lines = Vec::new();

        // Check file metadata for size changes or rotation
        if let Ok(meta) = std::fs::metadata(&self.path) {
            let current_len = meta.len();
            if current_len < self.last_offset {
                // File was rotated or truncated! Reopen from beginning.
                self.reader = None;
                self.last_offset = 0;
            }
        } else {
            // File might not exist yet
            self.reader = None;
            return lines;
        }

        if self.reader.is_none()
            && let Ok(file) = File::open(&self.path)
        {
            let mut reader = BufReader::new(file);
            if self.last_offset > 0 {
                let _ = reader.seek(SeekFrom::Start(self.last_offset));
            }
            self.reader = Some(reader);
        }

        let reader = match self.reader.as_mut() {
            Some(r) => r,
            None => return lines,
        };

        let mut offset = self.last_offset;
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break, // EOF reached for now
                Ok(_) => {
                    // Only consume complete lines ending with newline
                    if line.ends_with('\n') {
                        if let Ok(pos) = reader.stream_position() {
                            offset = pos;
                        }
                        let trimmed = line.trim_end_matches(['\r', '\n']).to_string();
                        if !trimmed.is_empty() {
                            lines.push(trimmed);
                        }
                    } else {
                        // Incomplete line written partially; rewind so next poll can read full line
                        let _ = reader.seek(SeekFrom::Start(offset));
                        break;
                    }
                }
                Err(_) => {
                    self.reader = None;
                    break;
                }
            }
        }
        self.last_offset = offset;

        lines
    }

    /// Reads all currently available new lines parsed as `LogLine`.
    pub fn poll_log_events(&mut self) -> Vec<LogLine> {
        self.poll_new_lines()
            .into_iter()
            .filter_map(|l| parse_line(&l))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn test_tailer_reads_appended_lines() {
        let dir = std::env::temp_dir().join("framepacer_tail_test");
        let _ = std::fs::create_dir_all(&dir);
        let log_file = dir.join("test.log");

        // Write initial content
        {
            let mut f = File::create(&log_file).unwrap();
            writeln!(f, "line 1").unwrap();
            writeln!(f, "line 2").unwrap();
        }

        // Tail starting at end
        let mut tailer = LogTailer::new(&log_file, true);
        assert!(tailer.poll_new_lines().is_empty());

        // Append line 3
        {
            let mut f = std::fs::OpenOptions::new()
                .append(true)
                .open(&log_file)
                .unwrap();
            writeln!(f, "line 3").unwrap();
        }

        let lines = tailer.poll_new_lines();
        assert_eq!(lines, vec!["line 3"]);

        // Rotation: rewrite file smaller
        {
            let mut f = File::create(&log_file).unwrap();
            writeln!(f, "rotated line 1").unwrap();
        }

        let lines = tailer.poll_new_lines();
        assert_eq!(lines, vec!["rotated line 1"]);

        let _ = std::fs::remove_file(log_file);
        let _ = std::fs::remove_dir(dir);
    }
}
