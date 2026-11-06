//! The holder's local child, with a helper that ties its whole process group
//! to the holder's stdin pipe. Startup identity is durable before the agent
//! is allowed to launch. Records retain the real agent spec;
//! only the launched command contains the internal helper invocation.

use crate::backend::{Exited, Instance, Stream};
use crate::lifecycle::Spec;
use crate::local::Local;
use anyhow::{Context as _, Result, bail};
use std::io::{BufRead as _, Write as _};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::Duration;

/// Shorter than the holder's `STARTUP`, so this error reaches deckd first.
const STARTUP_WAIT: Duration = Duration::from_secs(15);
const HEADER_LIMIT: u64 = 8192;

pub struct GuardedLocal {
    pub executable: PathBuf,
}

impl GuardedLocal {
    pub(crate) fn start(
        &mut self,
        spec: &Spec,
        exited: Exited,
        before_start: impl FnOnce(u32) -> Result<()>,
    ) -> Result<Box<dyn Instance>> {
        spec.argv.first().context("no agent command")?;
        let executable = self.executable.to_str().context("deckd executable path is not UTF-8")?;
        let mut argv = vec![executable.into(), "--child".into(), "--".into()];
        argv.extend(spec.argv.iter().cloned());
        let mut inner = Local::spawn(&Spec { argv, cwd: spec.cwd.clone(), github: None }, exited, None)?;
        let mut stream = inner.attach()?;
        before_start(inner.pid().context("helper has no pid")?)?;
        stream.writer.write_all(b"!")?;
        stream.writer.flush()?;
        let (ready, result) = mpsc::sync_channel(1);
        std::thread::Builder::new().name("deckd-child-startup".into()).spawn(move || {
            let outcome = (|| -> Result<Stream> {
                let mut line = Vec::new();
                std::io::Read::take(&mut stream.reader, HEADER_LIMIT).read_until(b'\n', &mut line)?;
                if line.last() != Some(&b'\n') {
                    bail!("child helper did not report agent startup");
                }
                let started: std::result::Result<(), String> = serde_json::from_slice(&line)?;
                started.map_err(anyhow::Error::msg)?;
                Ok(stream)
            })();
            let _ = ready.send(outcome);
        })?;
        let stream = match result.recv_timeout(STARTUP_WAIT) {
            Ok(result) => result?,
            Err(_) => {
                let _ = inner.stop(Duration::ZERO);
                bail!("timed out waiting for agent startup");
            }
        };
        Ok(Box::new(Guarded { inner, stream: Some(stream) }))
    }
}

struct Guarded {
    inner: Box<dyn Instance>,
    stream: Option<Stream>,
}

impl Instance for Guarded {
    fn pid(&self) -> Option<u32> {
        self.inner.pid()
    }

    fn attach(&mut self) -> Result<Stream> {
        self.stream.take().context("already attached: a local agent's stdio can be taken once")
    }

    fn stop(&mut self, grace: Duration) -> Result<String> {
        self.inner.stop(grace)
    }
}
