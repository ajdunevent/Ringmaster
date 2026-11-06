//! Collects everything the probe sees into one text file. The frame log is appended and flushed
//! as frames arrive, so an interrupted run leaves a usable file; `finish` rewrites it with the
//! summary on top.

use std::fs::File;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::Instant;

pub struct Report {
    path: PathBuf,
    start: Instant,
    inner: Mutex<Inner>,
}

struct Inner {
    file: Option<File>,
    log: Vec<String>,
    sections: Vec<(String, Vec<String>)>,
    findings: Vec<(String, String)>,
    frames: Vec<(f64, Vec<u8>)>,
    /// Opcodes counted instead of logged, for streams too large for the report.
    muted: Vec<(u8, usize)>,
}

impl Report {
    pub fn create(path: PathBuf) -> std::io::Result<Self> {
        let mut file = File::options().write(true).create_new(true).open(&path)?;
        writeln!(file, "ringprobe partial log (run did not finish; summary missing)\n")?;
        Ok(Self {
            path,
            start: Instant::now(),
            inner: Mutex::new(Inner {
                file: Some(file),
                log: Vec::new(),
                sections: Vec::new(),
                findings: Vec::new(),
                frames: Vec::new(),
                muted: Vec::new(),
            }),
        })
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn elapsed(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }

    fn push_line(&self, line: String) {
        let mut inner = self.inner.lock().unwrap();
        if let Some(f) = inner.file.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
        inner.log.push(line);
    }

    pub fn note(&self, text: impl AsRef<str>) {
        let line = format!("[{:9.3}] --  {}", self.elapsed(), text.as_ref());
        eprintln!("{line}");
        self.push_line(line);
    }

    pub fn tx(&self, bytes: &[u8], label: &str) {
        self.push_line(format!("[{:9.3}] TX  {:<48} {label}", self.elapsed(), crate::protocol::hex(bytes)));
    }

    /// Frames with opcode `op` are from now on counted, not logged; see `muted`.
    pub fn mute(&self, op: u8) {
        self.inner.lock().unwrap().muted.push((op, 0));
    }

    pub fn muted(&self) -> Vec<(u8, usize)> {
        self.inner.lock().unwrap().muted.clone()
    }

    pub fn rx(&self, bytes: &[u8], label: &str) -> f64 {
        let t = self.elapsed();
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(m) = inner.muted.iter_mut().find(|m| Some(&m.0) == bytes.first()) {
                m.1 += 1;
                return t;
            }
        }
        self.push_line(format!("[{t:9.3}] RX  {:<48} {label}", crate::protocol::hex(bytes)));
        self.inner.lock().unwrap().frames.push((t, bytes.to_vec()));
        t
    }

    pub fn frames(&self) -> Vec<(f64, Vec<u8>)> {
        self.inner.lock().unwrap().frames.clone()
    }

    pub fn section(&self, title: impl Into<String>, lines: Vec<String>) {
        self.inner.lock().unwrap().sections.push((title.into(), lines));
    }

    pub fn finding(&self, question: impl Into<String>, answer: impl Into<String>) {
        self.inner.lock().unwrap().findings.push((question.into(), answer.into()));
    }

    pub fn finish(&self, header: &[String]) -> std::io::Result<()> {
        let mut inner = self.inner.lock().unwrap();
        inner.file = None;
        let mut out = String::new();
        for line in header {
            out.push_str(line);
            out.push('\n');
        }
        out.push_str("\n## Findings\n\n");
        for (q, a) in &inner.findings {
            out.push_str(&format!("{q}\n    {}\n\n", a.replace('\n', "\n    ")));
        }
        for (title, lines) in &inner.sections {
            out.push_str(&format!("## {title}\n\n"));
            for l in lines {
                out.push_str(l);
                out.push('\n');
            }
            out.push('\n');
        }
        out.push_str("## Frame log\n\nTime is seconds since start. TX = phone to ring, RX = ring to phone.\n\n");
        for l in &inner.log {
            out.push_str(l);
            out.push('\n');
        }
        std::fs::write(&self.path, out)
    }
}
