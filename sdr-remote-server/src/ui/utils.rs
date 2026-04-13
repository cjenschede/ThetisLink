use std::time::Duration;

/// Run a blocking init function with a timeout.
/// Returns Err if the function hangs longer than the timeout.
pub(crate) fn with_timeout<T: Send + 'static>(
    timeout: Duration,
    f: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(f());
    });
    rx.recv_timeout(timeout)
        .unwrap_or_else(|_| Err("Timeout: COM poort reageert niet".to_string()))
}
