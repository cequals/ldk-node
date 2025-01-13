// This file is Copyright its original authors, visible in version control history.
//
// This file is licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license <LICENSE-MIT or
// http://opensource.org/licenses/MIT>, at your option. You may not use this file except in
// accordance with one or both of these licenses.

pub(crate) use lightning::util::logger::Logger;
pub(crate) use lightning::{log_bytes, log_debug, log_error, log_info, log_trace};

use lightning::util::logger::{Level, Record};

use chrono::Utc;

use std::fmt::Debug;
use std::fs;
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// A logger for LDK Node.
pub struct LdkNodeLogger {
	level: Level,
	writer: Box<dyn Fn(&Record) + Send + Sync>,
}

impl LdkNodeLogger {
	/// Creates a new `LdkNodeLogger`.
	pub fn new(level: Level, writer: Box<dyn Fn(&Record) + Send + Sync>) -> Self {
		Self { level, writer }
	}
}

impl Debug for LdkNodeLogger {
	fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
		write!(f, "LdkNodeLogger level: {}", self.level)
	}
}

impl Logger for LdkNodeLogger {
	fn log(&self, record: Record) {
		if record.level < self.level {
			return;
		}
		(self.writer)(&record)
	}
}

pub(crate) struct FilesystemLogWriter {
	log_file: Mutex<fs::File>,
}

impl FilesystemLogWriter {
	pub fn new(log_dir: String) -> Result<Self, ()> {
		let log_file_name =
			format!("ldk_node_{}.log", chrono::offset::Local::now().format("%Y_%m_%d"));
		let log_file_path = format!("{}/{}", log_dir, log_file_name);

		if let Some(parent_dir) = Path::new(&log_file_path).parent() {
			fs::create_dir_all(parent_dir)
				.map_err(|e| eprintln!("ERROR: Failed to create log parent directory: {}", e))?;

			// make sure the file exists.
			fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(&log_file_path)
				.map_err(|e| eprintln!("ERROR: Failed to open log file: {}", e))?;
		}

		let log_file = Mutex::new(
			fs::OpenOptions::new()
				.create(true)
				.append(true)
				.open(log_file_path.clone())
				.map_err(|e| eprintln!("ERROR: Failed to open log file: {}", e))?,
		);
		Ok(Self { log_file })
	}

	pub fn write(&self, log: &String) {
		self.log_file
			.lock()
			.expect("log file lock poisoned")
			.write_all(log.as_bytes())
			.expect("Failed to write to log file")
	}
}

pub(crate) fn default_format(record: &Record) -> String {
	let raw_log = record.args.to_string();
	format!(
		"{} {:<5} [{}:{}] {}\n",
		Utc::now().format("%Y-%m-%d %H:%M:%S"),
		record.level.to_string(),
		record.module_path,
		record.line,
		raw_log
	)
}
