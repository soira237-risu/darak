//! Optional bounded in-memory trace. Disk I/O belongs to main after the event loop.
use std::{
    io::{self, Write},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
    time::Duration,
};

const MAX_SAMPLES: usize = 100_000;
struct Sample {
    kind: &'static str,
    source_id: u64,
    generation: u64,
    phase: &'static str,
    elapsed: Duration,
}
struct Mapping {
    source_id: u64,
    generation: u64,
    fixture: String,
    query: String,
    scope: u32,
    mode: &'static str,
}
struct Buffer {
    samples: Vec<Sample>,
    mappings: Vec<Mapping>,
    overflow: u64,
    finished: bool,
}
pub struct PerfTrace {
    buffer: Arc<Mutex<Buffer>>,
}
pub struct PerfTraceRuntime {
    path: PathBuf,
    buffer: Arc<Mutex<Buffer>>,
}
impl PerfTraceRuntime {
    fn lock(&self) -> MutexGuard<'_, Buffer> {
        self.buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn new(path: PathBuf) -> (PerfTrace, Self) {
        let buffer = Arc::new(Mutex::new(Buffer {
            samples: Vec::with_capacity(MAX_SAMPLES),
            mappings: Vec::new(),
            overflow: 0,
            finished: false,
        }));
        (
            PerfTrace {
                buffer: buffer.clone(),
            },
            Self { path, buffer },
        )
    }
    pub fn write_csv(self) -> io::Result<()> {
        let b = self.lock();
        let mut out = io::BufWriter::new(std::fs::File::create(&self.path)?);
        writeln!(
            out,
            "schema,kind,source_id,generation,phase,elapsed_us,overflow_samples"
        )?;
        for s in &b.samples {
            writeln!(
                out,
                "1,{},{},{},{},{},0",
                s.kind,
                s.source_id,
                s.generation,
                s.phase,
                s.elapsed.as_micros()
            )?;
        }
        writeln!(out, "1,trace,0,0,overflow,0,{}", b.overflow)?;
        out.flush()?;
        let mut mappings = io::BufWriter::new(std::fs::File::create(
            self.path.with_extension("queries.csv"),
        )?);
        writeln!(
            mappings,
            "schema,fixture,source_id,generation,scope,mode,query"
        )?;
        for m in &b.mappings {
            writeln!(
                mappings,
                "1,{},{},{},{},{},{}",
                csv(&m.fixture),
                m.source_id,
                m.generation,
                m.scope,
                m.mode,
                csv(&m.query)
            )?;
        }
        mappings.flush()
    }
}
impl PerfTrace {
    fn lock(&self) -> MutexGuard<'_, Buffer> {
        self.buffer
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn release_observer(&self) -> crate::search_worker::ReleaseObserver {
        let trace = Self {
            buffer: Arc::clone(&self.buffer),
        };
        // record takes &mut only for the UI-facing API; the closure shares the bounded sink.
        Arc::new(move |release| {
            let mut trace = Self {
                buffer: Arc::clone(&trace.buffer),
            };
            trace.record(
                "search",
                release.doc_id,
                release.generation,
                "cancel_to_lease_release",
                release.released_at.duration_since(release.cancelled_at),
            );
        })
    }

    pub fn record(
        &mut self,
        kind: &'static str,
        source_id: u64,
        generation: u64,
        phase: &'static str,
        elapsed: Duration,
    ) {
        let mut b = self.lock();
        if b.finished {
            return;
        }
        if b.samples.len() == MAX_SAMPLES {
            b.overflow += 1;
            return;
        }
        b.samples.push(Sample {
            kind,
            source_id,
            generation,
            phase,
            elapsed,
        });
    }
    pub fn map_query(
        &mut self,
        source_id: u64,
        generation: u64,
        fixture: &str,
        query: &str,
        scope: u32,
        mode: &'static str,
    ) {
        let mut b = self.lock();
        if b.finished {
            return;
        }
        if b.mappings.len() == MAX_SAMPLES {
            b.overflow += 1;
            return;
        }
        b.mappings.push(Mapping {
            source_id,
            generation,
            fixture: fixture.into(),
            query: query.into(),
            scope,
            mode,
        });
    }
    #[cfg(test)]
    pub(crate) fn samples(&self, phase: &str) -> Vec<(u64, u64, Duration)> {
        self.lock()
            .samples
            .iter()
            .filter(|sample| sample.phase == phase)
            .map(|sample| (sample.source_id, sample.generation, sample.elapsed))
            .collect()
    }

    pub fn finish(&mut self) {
        self.lock().finished = true;
    }
}
fn csv(s: &str) -> String {
    format!("\"{}\"", s.replace('"', "\"\""))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DrawIdentity {
    pub key: crate::search_worker::SearchKey,
    pub generation: u64,
}
pub struct Measurement {
    pub key: crate::search_worker::SearchKey,
    pub generation: u64,
    pub input_at: std::time::Instant,
    pub drawn: bool,
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn trace_bounds_samples_and_finish_freezes_buffer() {
        let (mut trace, runtime) = PerfTraceRuntime::new(PathBuf::from("unused.csv"));
        for _ in 0..MAX_SAMPLES + 3 {
            trace.record("search", 2, 1, "draw", Duration::from_micros(9));
        }
        assert_eq!(runtime.lock().samples.len(), MAX_SAMPLES);
        assert_eq!(runtime.lock().overflow, 3);
        trace.finish();
        trace.record("search", 2, 1, "draw", Duration::ZERO);
        assert_eq!(runtime.lock().overflow, 3);
    }
    #[test]
    fn trace_writes_raw_mapping_and_overflow_after_finish() {
        let path = std::env::temp_dir().join(format!("darak-trace-{}.csv", std::process::id()));
        let (mut trace, runtime) = PerfTraceRuntime::new(path.clone());
        trace.record("search", 7, 3, "first_draw", Duration::from_micros(123));
        trace.map_query(7, 3, "fixture-abc", "자료,\"q\"\n", 1, "logical");
        trace.finish();
        runtime.write_csv().unwrap();
        let data = std::fs::read_to_string(&path).unwrap();
        assert!(data.contains("1,search,7,3,first_draw,123,0"));
        assert!(data.contains("1,trace,0,0,overflow,0,0"));
        let mapping = std::fs::read_to_string(path.with_extension("queries.csv")).unwrap();
        assert!(mapping.contains("1,\"fixture-abc\",7,3,1,logical,\"자료,\"\"q\"\"\n\""));
        std::fs::remove_file(&path).unwrap();
        std::fs::remove_file(path.with_extension("queries.csv")).unwrap();
    }
}
