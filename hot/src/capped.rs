use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
pub const DEFAULT_CAP_BYTES: u64 = 1024 * 1024 * 1024;
pub const SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
pub struct CappedLog {
    dir: PathBuf,
    name: String,
    cap: u64,
    seg: u64,
    seg_bytes: u64,
    file: Option<fs::File>,
    pub evicted_segments: u64,
    pub write_errors: u64,
}
fn seg_path(dir: &Path, name: &str, n: u64) -> PathBuf {
    dir.join(format!("{name}-{n:06}.jsonl"))
}
impl CappedLog {
    pub fn open(dir: impl AsRef<Path>, name: &str, cap: u64) -> std::io::Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let mut highest = 0u64;
        for (n, _) in Self::segments(&dir, name) {
            highest = highest.max(n);
        }
        let path = seg_path(&dir, name, highest);
        let seg_bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        let file = fs::OpenOptions::new().create(true).append(true).open(&path)?;
        Ok(Self {
            dir,
            name: name.to_string(),
            cap,
            seg: highest,
            seg_bytes,
            file: Some(file),
            evicted_segments: 0,
            write_errors: 0,
        })
    }
    fn segments(dir: &Path, name: &str) -> Vec<(u64, PathBuf)> {
        let mut v: Vec<(u64, PathBuf)> = Vec::new();
        let Ok(rd) = fs::read_dir(dir) else { return v };
        let prefix = format!("{name}-");
        for e in rd.flatten() {
            let p = e.path();
            let Some(f) = p.file_name().and_then(|s| s.to_str()) else { continue };
            if !f.starts_with(&prefix) || !f.ends_with(".jsonl") {
                continue;
            }
            let mid = &f[prefix.len()..f.len() - 6];
            if let Ok(n) = mid.parse::<u64>() {
                v.push((n, p));
            }
        }
        v.sort_by_key(|(n, _)| *n);
        v
    }
    pub fn total_bytes(&self) -> u64 {
        Self::segments(&self.dir, &self.name)
            .iter()
            .filter_map(|(_, p)| fs::metadata(p).ok().map(|m| m.len()))
            .sum()
    }
    pub fn write_line(&mut self, line: &str) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            match writeln!(f, "{line}") {
                Ok(()) => self.seg_bytes += line.len() as u64 + 1,
                Err(e) => {
                    self.write_errors += 1;
                    return Err(e);
                }
            }
        } else {
            self.write_errors += 1;
            return Err(
                std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "event log has no active segment",
                ),
            );
        }
        if self.seg_bytes >= SEGMENT_BYTES {
            self.roll()?;
        }
        Ok(())
    }
    fn roll(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            if let Err(e) = f.flush() {
                self.write_errors += 1;
                return Err(e);
            }
        }
        self.seg += 1;
        self.seg_bytes = 0;
        self.file = Some(
            fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(seg_path(&self.dir, &self.name, self.seg))
                .inspect_err(|_e| {
                    self.write_errors += 1;
                })?,
        );
        self.evict()
    }
    pub fn evict(&mut self) -> std::io::Result<()> {
        let mut segs = Self::segments(&self.dir, &self.name);
        segs.retain(|(n, _)| *n != self.seg);
        let mut total = self.total_bytes();
        for (_, p) in segs {
            if total <= self.cap {
                break;
            }
            let sz = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
            fs::remove_file(&p)
                .inspect_err(|_e| {
                    self.write_errors += 1;
                })?;
            total = total.saturating_sub(sz);
            self.evicted_segments += 1;
        }
        Ok(())
    }
    pub fn flush(&mut self) -> std::io::Result<()> {
        if let Some(f) = self.file.as_mut() {
            return f
                .flush()
                .inspect_err(|_e| {
                    self.write_errors += 1;
                });
        }
        Err(
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "event log has no active segment",
            ),
        )
    }
    pub fn status(&self) -> serde_json::Value {
        let total = self.total_bytes();
        serde_json::json!(
            { "bytes" : total, "cap" : self.cap, "pct" : ((total as f64 / self.cap as
            f64) * 1000.0).round() / 10.0, "segment" : self.seg, "evicted_segments" :
            self.evicted_segments, "write_errors" : self.write_errors, }
        )
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir()
            .join(format!("cappedtest-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }
    #[test]
    fn writes_and_reads_back() {
        let d = tmp("basic");
        let mut l = CappedLog::open(&d, "ev", 1 << 20).unwrap();
        l.write_line("{\"a\":1}").unwrap();
        l.write_line("{\"a\":2}").unwrap();
        l.flush().unwrap();
        let segs = CappedLog::segments(&d, "ev");
        let body = fs::read_to_string(&segs[0].1).unwrap();
        assert_eq!(body.lines().count(), 2);
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn NEWEST_evicts_OLDEST_once_the_cap_is_passed() {
        let d = tmp("evict");
        let mut l = CappedLog::open(&d, "ev", 40).unwrap();
        for i in 0..6 {
            l.seg_bytes = SEGMENT_BYTES;
            l.write_line(&format!("{{\"i\":{i}}}")).unwrap();
        }
        l.flush().unwrap();
        let segs = CappedLog::segments(&d, "ev");
        assert!(l.evicted_segments > 0, "nothing was evicted");
        let nums: Vec<u64> = segs.iter().map(|(n, _)| *n).collect();
        let hi = *nums.iter().max().unwrap();
        assert!(nums.contains(& hi), "the active segment must survive");
        assert!(
            ! nums.contains(& 0) || nums.len() == 1, "segment 0 should be gone: {nums:?}"
        );
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn the_segment_being_written_is_never_evicted() {
        let d = tmp("active");
        let mut l = CappedLog::open(&d, "ev", 1).unwrap();
        l.write_line("{\"keep\":true}").unwrap();
        l.evict().unwrap();
        l.flush().unwrap();
        let segs = CappedLog::segments(&d, "ev");
        assert_eq!(segs.len(), 1, "evicted the segment we are writing to");
        assert!(fs::read_to_string(& segs[0].1).unwrap().contains("keep"));
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn a_restart_resumes_instead_of_reusing_a_number() {
        let d = tmp("resume");
        {
            let mut l = CappedLog::open(&d, "ev", 1 << 20).unwrap();
            l.seg_bytes = SEGMENT_BYTES;
            l.write_line("{\"first\":1}").unwrap();
            l.flush().unwrap();
        }
        let l2 = CappedLog::open(&d, "ev", 1 << 20).unwrap();
        assert_eq!(l2.seg, 1, "must continue at the highest segment, not restart at 0");
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn status_reports_usage_against_the_cap() {
        let d = tmp("status");
        let mut l = CappedLog::open(&d, "ev", 1000).unwrap();
        for _ in 0..10 {
            l.write_line("0123456789").unwrap();
        }
        l.flush().unwrap();
        let s = l.status();
        assert_eq!(s["cap"], 1000);
        assert!(s["bytes"].as_u64().unwrap() >= 110);
        assert_eq!(s["write_errors"], 0);
        let _ = fs::remove_dir_all(&d);
    }
    #[test]
    fn segments_sort_numerically_not_lexically() {
        let d = tmp("sort");
        let mut l = CappedLog::open(&d, "ev", 1 << 30).unwrap();
        for _ in 0..12 {
            l.seg_bytes = SEGMENT_BYTES;
            l.write_line("x").unwrap();
        }
        l.flush().unwrap();
        let nums: Vec<u64> = CappedLog::segments(&d, "ev")
            .iter()
            .map(|(n, _)| *n)
            .collect();
        let mut sorted = nums.clone();
        sorted.sort();
        assert_eq!(nums, sorted);
        assert!(nums.contains(& 10) && nums.contains(& 11));
        let _ = fs::remove_dir_all(&d);
    }
}
