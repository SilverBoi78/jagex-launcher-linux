//! A shared, bounded log that the UI renders and background threads append to.
//!
//! Download progress rewrites the newest line rather than appending, so a download does
//! not scroll the rest of the log away.

use std::sync::{Arc, Mutex};

const MAX_LINES: usize = 500;

#[derive(Clone, Default)]
pub struct Log {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Log {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn info(&self, message: impl Into<String>) {
        let mut lines = self.lines.lock().unwrap();
        lines.push(message.into());
        if lines.len() > MAX_LINES {
            let excess = lines.len() - MAX_LINES;
            lines.drain(..excess);
        }
    }

    pub fn error(&self, message: impl std::fmt::Display) {
        self.info(format!("error: {message}"));
    }

    /// Replaces the newest line, for progress that updates in place.
    pub fn replace_last(&self, message: impl Into<String>) {
        let mut lines = self.lines.lock().unwrap();
        match lines.last_mut() {
            Some(last) => *last = message.into(),
            None => lines.push(message.into()),
        }
    }

    pub fn snapshot(&self) -> Vec<String> {
        self.lines.lock().unwrap().clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replace_last_overwrites_rather_than_appends() {
        let log = Log::new();
        log.info("downloading...");
        log.replace_last("downloading... 50%");
        log.replace_last("downloading... done");
        assert_eq!(log.snapshot(), vec!["downloading... done"]);
    }

    #[test]
    fn replace_last_on_an_empty_log_appends() {
        let log = Log::new();
        log.replace_last("first");
        assert_eq!(log.snapshot(), vec!["first"]);
    }

    #[test]
    fn old_lines_are_dropped_once_the_cap_is_reached() {
        let log = Log::new();
        for i in 0..MAX_LINES + 50 {
            log.info(i.to_string());
        }
        let lines = log.snapshot();
        assert_eq!(lines.len(), MAX_LINES);
        // the newest lines are the ones kept
        assert_eq!(lines.last().unwrap(), &(MAX_LINES + 49).to_string());
        assert_eq!(lines.first().unwrap(), &50.to_string());
    }

    #[test]
    fn is_shared_between_clones() {
        let log = Log::new();
        let other = log.clone();
        other.info("from the worker thread");
        assert_eq!(log.snapshot(), vec!["from the worker thread"]);
    }
}
