//! Request-owned CLI presentation. Attached requests never write to host stdout.
use anyhow::{Result, anyhow};
use bubblepaw_core::{CancellationToken, EventKind, HarnessEvent};
use serde::{Deserialize, Serialize};
use std::io::Write;
use tokio::sync::mpsc;

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stream {
    Stdout,
    Stderr,
}

pub enum Record {
    Text { stream: Stream, text: String },
    Event(Box<HarnessEvent>),
}

#[derive(Clone, Default)]
pub struct Output {
    remote: Option<(mpsc::Sender<Record>, CancellationToken)>,
}

impl Output {
    pub fn remote(sender: mpsc::Sender<Record>, cancel: CancellationToken) -> Self {
        Self {
            remote: Some((sender, cancel)),
        }
    }

    fn send(&self, record: Record) -> Result<()> {
        let (sender, cancel) = self.remote.as_ref().expect("remote output");
        sender.try_send(record).map_err(|_| {
            cancel.cancel();
            anyhow!("attached client stopped consuming output")
        })
    }

    pub fn write(&self, stream: Stream, text: String) -> Result<()> {
        if self.remote.is_some() {
            return self.send(Record::Text { stream, text });
        }
        match stream {
            Stream::Stdout => {
                let mut writer = std::io::stdout().lock();
                writer.write_all(text.as_bytes())?;
                writer.flush()?;
            }
            Stream::Stderr => {
                let mut writer = std::io::stderr().lock();
                writer.write_all(text.as_bytes())?;
                writer.flush()?;
            }
        }
        Ok(())
    }

    pub fn stdout(&self, text: impl Into<String>) -> Result<()> {
        self.write(Stream::Stdout, text.into())
    }

    pub fn stderr(&self, text: impl Into<String>) -> Result<()> {
        self.write(Stream::Stderr, text.into())
    }

    pub fn json(&self, value: &impl Serialize) -> Result<()> {
        self.stdout(format!("{}\n", serde_json::to_string(value)?))
    }

    pub fn event(&self, event: &HarnessEvent, json: bool) -> Result<()> {
        if self.remote.is_some() {
            return self.send(Record::Event(Box::new(event.clone())));
        }
        if json {
            return self.json(event);
        }
        match &event.kind {
            EventKind::Paragraph { text } => self.stdout(format!("{text}\n\n")),
            EventKind::ToolStarted { call } => self.stderr(format!("[{}]\n", call.name)),
            EventKind::ToolFinished { name, result, .. } if result.is_error => {
                self.stderr(format!("[{name}: {}]\n", result.content))
            }
            _ => Ok(()),
        }
    }
}
