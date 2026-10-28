use std::{
    io::Write,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
struct Captured(Arc<Mutex<Vec<u8>>>);
impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

pub fn capture_warnings() -> Arc<Mutex<Vec<u8>>> {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let writer = Captured(captured.clone());
    // A single global subscriber avoids callsite-filter races between this
    // binary's parallel tests; only the malformed metadata test emits this event.
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(move || writer.clone())
        .finish();
    tracing::subscriber::set_global_default(subscriber).unwrap();
    captured
}
