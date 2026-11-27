//! Copying a file to the server for a prompt to name by path, as Desktop's attachment destination
//! does (`composer/attachments/destination.ts`): the body streams into the server's temporary
//! directory, one folder per upload, and reports how much has gone as it goes, as an
//! `XMLHttpRequest`'s `upload.onprogress` does there.

use std::io::Read;

use serde_json::Value;

use super::{Server, encode_component, message_id};

/// Why an upload didn't land.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum UploadError {
    /// `sent` asked it to stop (cancelled, or stalled).
    Aborted,
    Failed(String),
}

impl Server {
    /// Streams `size` bytes from `body` into the server's temporary directory as `name` and
    /// returns the path there. `sent` hears the bytes sent so far after each piece goes; when it
    /// answers false the request ends there. Like Desktop's, the request has no time limit of its
    /// own: the caller decides when a body that stops moving has stalled.
    pub fn upload(&self, directory: &str, name: &str, body: &mut dyn Read, size: u64, sent: &mut dyn FnMut(u64) -> bool) -> Result<String, UploadError> {
        // What Desktop's request rejects with (`destination.ts:60-64`): its status, or nothing more.
        let failed = |status: Option<u16>| UploadError::Failed(status.map_or_else(|| "Upload failed".to_owned(), |n| format!("Upload failed with status {n}")));
        let info = self.get("/api/info").map_err(|e| UploadError::Failed(e.to_string()))?;
        let tmp = info["paths"]["tmp"].as_str().ok_or_else(|| UploadError::Failed("GET /api/info: no paths.tmp".into()))?;
        let target = format!("{}/uploads/{}/{name}", tmp.trim_end_matches('/'), message_id());
        let path = format!(
            "/api/experimental/fs/write?location%5Bdirectory%5D={}&path={}",
            encode_component(directory),
            encode_component(&target)
        );
        let req = self
            .dress(self.agent().post(format!("{}{path}", self.base)), &[], None)
            .header("Content-Type", "application/octet-stream")
            .header("Content-Length", size.to_string());
        let mut reader = Progress { body, sent: 0, report: sent, stopped: false };
        let result = req.send(ureq::SendBody::from_reader(&mut reader));
        if reader.stopped {
            return Err(UploadError::Aborted);
        }
        let mut resp = result.map_err(|_| failed(None))?;
        let status = resp.status().as_u16();
        if !(200..300).contains(&status) {
            return Err(failed(Some(status)));
        }
        let text = resp.body_mut().read_to_string().map_err(|_| failed(None))?;
        let v: Value = serde_json::from_str(&text).map_err(|_| failed(None))?;
        v["data"]["path"].as_str().map(str::to_owned).ok_or_else(|| failed(None))
    }
}

/// The body as it's read for sending, counting what has gone.
struct Progress<'a> {
    body: &'a mut dyn Read,
    sent: u64,
    report: &'a mut dyn FnMut(u64) -> bool,
    stopped: bool,
}

impl Read for Progress<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.stopped {
            return Err(std::io::Error::other("upload aborted"));
        }
        let n = self.body.read(buf)?;
        self.sent += n as u64;
        if !(self.report)(self.sent) {
            self.stopped = true;
            return Err(std::io::Error::other("upload aborted"));
        }
        Ok(n)
    }
}
