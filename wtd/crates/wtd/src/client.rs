//! Blocking pipe client for short-lived callers (hook, CLI, tray, MCP, `wtd run`).
//!
//! A synchronous pipe handle serializes reads and writes, so a caller that both waits for pushes and
//! sends requests should use two connections (see the tray).

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use serde::Serialize;
use serde_json::Value;
use wtd_core::protocol::{method, Request, ServerLine, PROTOCOL_VERSION};

const ERROR_FILE_NOT_FOUND: i32 = 2;
const ERROR_PIPE_BUSY: i32 = 231;

pub struct Client {
    reader: BufReader<File>,
    writer: File,
    next_id: u64,
}

impl Client {
    /// Connect to the running daemon; `Ok(None)` when it isn't running.
    pub fn connect() -> Result<Option<Client>> {
        let name = crate::paths::pipe_name();
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            match OpenOptions::new().read(true).write(true).open(&name) {
                Ok(f) => {
                    let writer = f.try_clone()?;
                    return Ok(Some(Client { reader: BufReader::new(f), writer, next_id: 1 }));
                }
                Err(e) if e.raw_os_error() == Some(ERROR_FILE_NOT_FOUND) => return Ok(None),
                // All instances busy: the daemon re-arms a new instance right after each accept.
                Err(e) if e.raw_os_error() == Some(ERROR_PIPE_BUSY) && Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => return Err(e).context("connecting to the wtd daemon"),
            }
        }
    }

    pub fn connect_required() -> Result<Client> {
        Client::connect()?.ok_or_else(|| anyhow!("daemon not running — start it from the tray icon or the fleet panel (or `wtd daemon start`)"))
    }

    fn send(&mut self, req: &Request) -> Result<()> {
        let mut line = serde_json::to_vec(req)?;
        line.push(b'\n');
        self.writer.write_all(&line)?;
        Ok(())
    }

    /// Fire-and-forget.
    pub fn notify(&mut self, method: &str, params: impl Serialize) -> Result<()> {
        self.send(&Request { id: None, method: method.into(), params: serde_json::to_value(params)? })
    }

    /// Send a request and wait for its response (pushes arriving meanwhile are dropped).
    pub fn request(&mut self, method: &str, params: impl Serialize) -> Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        self.send(&Request { id: Some(id), method: method.into(), params: serde_json::to_value(params)? })?;
        loop {
            match self.read()? {
                Some(ServerLine::Response(r)) if r.id == id => {
                    if let Some(e) = r.error {
                        bail!(e);
                    }
                    return Ok(r.result.unwrap_or(Value::Null));
                }
                Some(_) => continue,
                None => bail!("daemon closed the connection"),
            }
        }
    }

    pub fn hello(&mut self, client: &str) -> Result<Value> {
        self.request(method::HELLO, serde_json::json!({ "client": client, "protocol": PROTOCOL_VERSION }))
    }

    /// Next line from the daemon; `None` on disconnect.
    pub fn read(&mut self) -> Result<Option<ServerLine>> {
        let mut line = String::new();
        loop {
            line.clear();
            if self.reader.read_line(&mut line)? == 0 {
                return Ok(None);
            }
            if line.trim().is_empty() {
                continue;
            }
            return Ok(Some(serde_json::from_str(&line).with_context(|| format!("bad line from daemon: {line}"))?));
        }
    }
}
