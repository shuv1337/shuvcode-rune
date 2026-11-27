//! Page-fetch worker lanes: pulls newest/history jobs from the shared queue, frees the raw JSON
//! tree before decoding scaled thumbnails, and collects the worker's mimalloc heap before parking.

use std::sync::{Arc, PoisonError, mpsc::Sender};
use std::time::Duration;

use opencode_kit::model;
use rune::Waker;
use rune::time::Instant;
use serde_json::Value;

use super::{ENTRIES, Fetch, Job, LaneHealthEvent, MEMORY_BYTES, Page, PageResult, QueueEvent, QueueState, Shared, bridge, now};

/// Selects the next job for `foreground`'s lane: foreground picks newest-first; background serves
/// explicit history/bridge page reads ahead of single-message re-reads and speculative prefetches.
fn pick_job(jobs: &std::collections::VecDeque<Job>, foreground: bool) -> Option<usize> {
    if foreground {
        jobs.iter().rposition(|j| j.foreground)
    } else {
        jobs.iter()
            .position(|j| !j.foreground && j.cursor.is_some() && !j.one)
            .or_else(|| jobs.iter().position(|j| !j.foreground && j.cursor.is_some()))
            .or_else(|| jobs.iter().rposition(|j| !j.foreground))
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub(super) fn worker(q: Shared, fetch: Fetch, out: Sender<PageResult>, wake: Waker, images: Arc<rune::ImageCache>, foreground: bool) {
    let mut worked = false;
    loop {
        let job = {
            let mut state = q.0.lock().unwrap_or_else(PoisonError::into_inner);
            loop {
                let QueueState::Running(inner) = &mut *state else { return };
                if let Some(i) = pick_job(&inner.jobs, foreground)
                    && let Some(job) = inner.jobs.remove(i)
                {
                    inner.active.insert((job.key.clone(), job.cursor.clone()));
                    break job;
                }
                if worked {
                    // Return cross-thread freed blocks & retired pages before parking idle.
                    drop(state);
                    crate::collect_thread_heap();
                    worked = false;
                    state = q.0.lock().unwrap_or_else(PoisonError::into_inner);
                    continue;
                }
                state = q.1.wait(state).unwrap_or_else(PoisonError::into_inner);
            }
        };
        if !read(&q, &fetch, &out, &wake, &images, foreground, job) { return; }
        worked = true;
    }
}

/// The next job for the lane, if one waits: the web's lanes take what's queued and stop.
#[cfg(target_arch = "wasm32")]
pub(super) fn take_now(q: &Shared, foreground: bool) -> Option<Job> {
    let mut state = q.0.lock().unwrap_or_else(PoisonError::into_inner);
    let QueueState::Running(inner) = &mut *state else { return None };
    let i = pick_job(&inner.jobs, foreground)?;
    let job = inner.jobs.remove(i)?;
    inner.active.insert((job.key.clone(), job.cursor.clone()));
    Some(job)
}

/// Reads `job` from the server and hands each page to `out`; false when the pool stopped.
#[allow(clippy::too_many_arguments)]
pub(super) fn read(q: &Shared, fetch: &Fetch, out: &Sender<PageResult>, wake: &Waker, images: &rune::ImageCache, foreground: bool, job: Job) -> bool {
    {
        let publish = |value: Value, mut read: crate::telemetry::Read| {
            let parsing = Instant::now();
            let messages = if job.one { model::parse_message(&value["data"]).into_iter().collect() } else { model::parse_messages(&value) };
            let stubs = crate::context_window::stubs(&value);
            let cursor = value["cursor"]["next"].as_str().map(str::to_owned);
            drop(value); // Free the JSON tree before decoding images to cut peak RAM.
            let preparing = Instant::now();
            let prepared = if job.cursor.is_none() { crate::attachments::prepare_images(&messages, images) } else { vec![] };
            let done = Instant::now();
            (read.parse, read.images, read.messages) = (Some((parsing, preparing)), Some((preparing, done)), messages.len());
            (read.inline_images, read.inline_image_bytes) = crate::attachments::inline_images(&messages);
            read.prepared = prepared.iter().map(|p| p.handle()).collect();
            read.thread = std::thread::current().name().unwrap_or("").to_owned();
            let wire = if read.bytes > 0 { read.bytes } else { read.inline_image_bytes + messages.len() * 1024 };
            let page = Arc::new(Page {
                key: job.key.clone(), messages, stubs, asked: job.limit,
                cursor, history: job.cursor.is_some(), read_of: bridge::ReadOf::of(job.cursor.as_deref(), job.one),
                bytes: wire + prepared.iter().map(|p| p.weight()).sum::<usize>(),
                images: prepared,
                read,
            });
            {
                let mut state = q.0.lock().unwrap_or_else(PoisonError::into_inner);
                if let QueueState::Running(inner) = &mut *state {
                    if !page.history
                        && inner.ready.iter().all(|p| p.key.id != page.key.id || p.key.updated <= page.key.updated)
                    {
                        inner.ready.retain(|p| p.key.id != page.key.id);
                        inner.ready.push_back(page.clone());
                        while inner.ready.len() > ENTRIES || inner.ready.iter().map(|p| p.bytes).sum::<usize>() > MEMORY_BYTES {
                            inner.ready.pop_front();
                        }
                    }
                    // Clear active before waking the UI thread so an immediate re-enqueue cannot race.
                    inner.active.remove(&(job.key.clone(), job.cursor.clone()));
                }
            }
            if out.send(Ok(page)).is_ok() { wake(); }
        };
        let path = match &job.cursor {
            Some(message) if job.one => format!("/api/session/{}/message/{message}", job.key.id),
            Some(cursor) => format!("/api/session/{}/message?limit={}&cursor={}", job.key.id, job.limit, crate::switcher::encode(cursor)),
            None => format!("/api/session/{}/message?limit={}&order=desc", job.key.id, job.limit),
        };
        let first = foreground && job.cursor.is_none();
        if first { rune::marks::mark("session.read.sent"); }
        let mut start = Instant::now();
        crate::telemetry::take_read();
        let mut answer = fetch(&path);
        for attempt in 0.. {
            let Err(error) = &answer else { break };
            if !foreground || !error.transient() { break; }
            eprintln!("opencode-rune: session read failed, retrying: {error}");
            q.0.lock().unwrap_or_else(PoisonError::into_inner).step(QueueEvent::Health(LaneHealthEvent::TransientFailure(now())));
            wake();
            if !pause(q, backoff(attempt)) { return false; }
            start = Instant::now();
            crate::telemetry::take_read();
            answer = fetch(&path);
        }
        if answer.is_ok() { q.0.lock().unwrap_or_else(PoisonError::into_inner).step(QueueEvent::Health(LaneHealthEvent::Answered)); }
        match answer {
            Ok(value) => {
                if first { rune::marks::mark("session.read.received"); }
                let end = Instant::now();
                let mut read = crate::telemetry::Read { source: "server", fetch: Some((start, end)), ..Default::default() };
                if let Some((first_byte, bytes, parsing)) = crate::telemetry::take_read() {
                    (read.first_byte, read.bytes, read.json) = (Some(first_byte), bytes, Some((parsing, end)));
                    read.fetch = Some((start, parsing));
                }
                publish(value, read);
            }
            Err(error) => {
                {
                    let mut state = q.0.lock().unwrap_or_else(PoisonError::into_inner);
                    if let QueueState::Running(inner) = &mut *state {
                        if job.cursor.is_none() {
                            inner.failed.insert(job.key.clone(), Instant::now() + Duration::from_secs(30));
                        }
                        inner.active.remove(&(job.key.clone(), job.cursor.clone()));
                    }
                }
                let read_of = bridge::ReadOf::of(job.cursor.as_deref(), job.one);
                if out.send(Err((job.key.clone(), read_of, error))).is_ok() { wake(); }
            }
        }
    }
    true
}

/// 2 s, 4 s, then 8 s between reads.
fn backoff(attempt: u32) -> Duration {
    let unit = Duration::from_secs(1);
    unit * (2u32 << attempt.min(2))
}

/// Sleeps `total`; false if the pool stopped meanwhile.
fn pause(q: &Shared, total: Duration) -> bool {
    let until = Instant::now() + total;
    while Instant::now() < until {
        if !q.0.lock().unwrap_or_else(PoisonError::into_inner).is_running() { return false; }
        rune::task::sleep(Duration::from_millis(10).min(until - Instant::now()));
    }
    q.0.lock().unwrap_or_else(PoisonError::into_inner).is_running()
}
