use std::{
    fs::{self, File},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        mpsc::{self, Receiver, Sender},
    },
    thread,
};

use reqwest::blocking::{Client, Response};

use crate::manifest::WallpaperAsset;

#[derive(Debug, Clone)]
pub struct DownloadPlan {
    pub assets: Vec<WallpaperAsset>,
    pub output_dir: PathBuf,
    pub threads: usize,
}

#[derive(Debug)]
pub enum DownloadEvent {
    Started {
        index: usize,
        total: usize,
        title: String,
        file_name: String,
        total_bytes: Option<u64>,
    },
    Progress {
        index: usize,
        bytes_downloaded: u64,
        total_bytes: Option<u64>,
    },
    Finished {
        index: usize,
        path: PathBuf,
    },
    Skipped {
        index: usize,
        path: PathBuf,
    },
    Error {
        index: usize,
        message: String,
    },
    Complete,
}

pub fn start_downloads(plan: DownloadPlan, tx: Sender<DownloadEvent>) {
    let fallback_plan = plan.clone();
    let fallback_tx = tx.clone();
    match thread::Builder::new().spawn(move || {
        run_downloads(&plan, &tx);
        let _ = tx.send(DownloadEvent::Complete);
    }) {
        Ok(_) => {}
        Err(err) => {
            for (index, asset) in fallback_plan.assets.iter().enumerate() {
                send_error(
                    &fallback_tx,
                    index,
                    format!(
                        "failed to start download queue for {}: {}",
                        asset.title, err
                    ),
                );
            }
            let _ = fallback_tx.send(DownloadEvent::Complete);
        }
    }
}

fn run_downloads(plan: &DownloadPlan, tx: &Sender<DownloadEvent>) {
    if let Err(err) = fs::create_dir_all(&plan.output_dir) {
        for (index, asset) in plan.assets.iter().enumerate() {
            send_error(
                tx,
                index,
                format!("failed to prepare {}: {}", asset.title, err),
            );
        }
        return;
    }

    if plan.assets.is_empty() {
        return;
    }

    let client = match build_client() {
        Ok(client) => client,
        Err(err) => {
            for (index, asset) in plan.assets.iter().enumerate() {
                send_error(
                    tx,
                    index,
                    format!("failed to prepare {} ({}): {}", asset.title, asset.url, err),
                );
            }
            return;
        }
    };

    let total = plan.assets.len();
    let worker_count = plan.threads.max(1).min(total);
    let (job_tx, job_rx) = mpsc::channel::<DownloadJob>();
    for (index, asset) in plan.assets.iter().cloned().enumerate() {
        job_tx
            .send(DownloadJob { index, asset })
            .expect("download workers are started after jobs are queued");
    }
    drop(job_tx);

    let shared_rx = Arc::new(Mutex::new(job_rx));
    let mut handles = Vec::with_capacity(worker_count.saturating_sub(1));

    for _ in 1..worker_count {
        let worker_rx = Arc::clone(&shared_rx);
        let worker_tx = tx.clone();
        let worker_client = client.clone();
        let output_dir = plan.output_dir.clone();
        if let Ok(handle) = thread::Builder::new().spawn(move || {
            worker_loop(total, output_dir, worker_client, worker_rx, worker_tx);
        }) {
            handles.push(handle);
        }
    }

    worker_loop(
        total,
        plan.output_dir.clone(),
        client,
        shared_rx,
        tx.clone(),
    );

    for handle in handles {
        let _ = handle.join();
    }
}

#[derive(Debug)]
struct DownloadJob {
    index: usize,
    asset: WallpaperAsset,
}

fn worker_loop(
    total: usize,
    output_dir: PathBuf,
    client: Client,
    receiver: Arc<Mutex<Receiver<DownloadJob>>>,
    tx: Sender<DownloadEvent>,
) {
    loop {
        let job = {
            let Ok(lock) = receiver.lock() else {
                return;
            };
            lock.recv()
        };

        let Ok(job) = job else {
            return;
        };
        let index = job.index;
        let temp_path = output_dir.join(format!("{}.part", job.asset.file_name));

        if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            process_job(total, &output_dir, &client, &tx, job);
        }))
        .is_err()
        {
            let _ = fs::remove_file(temp_path);
            send_error(&tx, index, "a download worker panicked".to_owned());
        }
    }
}

fn process_job(
    total: usize,
    output_dir: &Path,
    client: &Client,
    tx: &Sender<DownloadEvent>,
    job: DownloadJob,
) {
    let DownloadJob { index, asset } = job;
    let target_path = output_dir.join(&asset.file_name);
    let temp_path = target_path.with_file_name(format!("{}.part", asset.file_name));
    let expected_length = remote_length(client, &asset.url);

    if is_up_to_date(&target_path, expected_length) {
        let _ = tx.send(DownloadEvent::Skipped {
            index,
            path: target_path,
        });
        return;
    }

    let response = client.get(&asset.url).send();
    let mut response = match response {
        Ok(resp) => resp,
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            send_error(
                tx,
                index,
                format!("failed to request {} ({}): {}", asset.title, asset.url, err),
            );
            return;
        }
    };

    if !response.status().is_success() {
        let status = response.status();
        let _ = fs::remove_file(&temp_path);
        send_error(
            tx,
            index,
            format!(
                "server returned {} for {} ({})",
                status, asset.title, asset.url
            ),
        );
        return;
    }

    let total_bytes = declared_content_length(&response).or(expected_length);
    let _ = tx.send(DownloadEvent::Started {
        index,
        total,
        title: asset.title.clone(),
        file_name: asset.file_name.clone(),
        total_bytes,
    });

    match download_stream(&mut response, &temp_path, total_bytes, index, tx) {
        Ok(()) => {
            if let Err(err) = fs::rename(&temp_path, &target_path) {
                let _ = fs::remove_file(&temp_path);
                send_error(
                    tx,
                    index,
                    format!(
                        "failed to finalize {} ({}): {}",
                        asset.title, asset.url, err
                    ),
                );
                return;
            }
            let _ = tx.send(DownloadEvent::Finished {
                index,
                path: target_path,
            });
        }
        Err(err) => {
            let _ = fs::remove_file(&temp_path);
            send_error(
                tx,
                index,
                format!(
                    "failed to download {} ({}): {}",
                    asset.title, asset.url, err
                ),
            );
        }
    }
}

fn build_client() -> Result<Client, reqwest::Error> {
    let builder = Client::builder();
    #[cfg(test)]
    let builder = builder.no_proxy();
    builder
        .user_agent("awm/0.1")
        .danger_accept_invalid_certs(true)
        .build()
}

fn send_error(tx: &Sender<DownloadEvent>, index: usize, message: String) {
    let _ = tx.send(DownloadEvent::Error { index, message });
}

fn remote_length(client: &Client, url: &str) -> Option<u64> {
    let response = client.head(url).send().ok()?;
    if !response.status().is_success() {
        return None;
    }
    declared_content_length(&response)
}

fn declared_content_length(response: &Response) -> Option<u64> {
    response
        .headers()
        .get(reqwest::header::CONTENT_LENGTH)?
        .to_str()
        .ok()?
        .parse()
        .ok()
}

fn is_up_to_date(path: &Path, expected_length: Option<u64>) -> bool {
    let Some(expected_length) = expected_length else {
        return false;
    };
    path.is_file()
        && fs::metadata(path)
            .map(|metadata| metadata.len() == expected_length)
            .unwrap_or(false)
}

fn download_stream<R: Read>(
    reader: &mut R,
    temp_path: &Path,
    total_bytes: Option<u64>,
    index: usize,
    tx: &Sender<DownloadEvent>,
) -> Result<(), std::io::Error> {
    let mut file = File::create(temp_path)?;
    let mut buffer = [0u8; 64 * 1024];
    let mut downloaded = 0u64;

    loop {
        let bytes_read = reader.read(&mut buffer)?;
        if bytes_read == 0 {
            break;
        }
        let next_downloaded = downloaded.saturating_add(bytes_read as u64);
        if let Some(total) = total_bytes
            && next_downloaded > total
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "response exceeded its reported size",
            ));
        }
        file.write_all(&buffer[..bytes_read])?;
        downloaded = next_downloaded;
        let _ = tx.send(DownloadEvent::Progress {
            index,
            bytes_downloaded: downloaded,
            total_bytes,
        });
    }

    if let Some(total) = total_bytes
        && downloaded != total
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            format!("response ended at {downloaded} of {total} bytes"),
        ));
    }

    file.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{
        fs,
        io::Cursor,
        net::{TcpListener, TcpStream},
        sync::{
            Arc, Condvar, Mutex,
            atomic::{AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    static NEXT_TEST_DIR: AtomicUsize = AtomicUsize::new(0);

    #[derive(Clone)]
    struct TestServer {
        body: Vec<u8>,
        head_length: Option<u64>,
        status: &'static str,
        probe: Option<Arc<ConcurrencyProbe>>,
    }

    struct ConcurrencyProbe {
        active: AtomicUsize,
        maximum: AtomicUsize,
        gate: ConcurrencyGate,
    }

    struct ConcurrencyGate {
        state: Mutex<GateState>,
        released: Condvar,
    }

    struct GateState {
        waiting: usize,
        released: bool,
    }

    impl ConcurrencyGate {
        fn new() -> Self {
            Self {
                state: Mutex::new(GateState {
                    waiting: 0,
                    released: false,
                }),
                released: Condvar::new(),
            }
        }

        fn wait_for_pair(&self) {
            let mut state = self.state.lock().expect("lock concurrency gate");
            if state.released {
                return;
            }

            state.waiting += 1;
            if state.waiting >= 2 {
                state.released = true;
                self.released.notify_all();
                return;
            }

            let (mut state, _) = self
                .released
                .wait_timeout_while(state, Duration::from_secs(2), |state| !state.released)
                .expect("wait for concurrency gate");
            if !state.released {
                state.released = true;
                self.released.notify_all();
            }
        }
    }

    impl TestServer {
        fn new(body: Vec<u8>, head_length: Option<u64>, status: &'static str) -> Self {
            Self {
                body,
                head_length,
                status,
                probe: None,
            }
        }

        fn with_probe(mut self, probe: Arc<ConcurrencyProbe>) -> Self {
            self.probe = Some(probe);
            self
        }
    }

    fn wallpaper_asset(url: String) -> WallpaperAsset {
        WallpaperAsset {
            id: "sample".to_owned(),
            title: "Sample".to_owned(),
            description: None,
            url,
            file_name: "sample.mov".to_owned(),
            extension: ".mov".to_owned(),
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let suffix = NEXT_TEST_DIR.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir()
                .join(format!("awm-download-test-{}-{suffix}", std::process::id()));
            let _ = fs::remove_dir_all(&path);
            fs::create_dir_all(&path).expect("create temp dir");
            Self(path)
        }
    }

    impl std::ops::Deref for TestDir {
        type Target = Path;

        fn deref(&self) -> &Self::Target {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn temp_dir() -> TestDir {
        TestDir::new()
    }

    fn spawn_server(
        response: TestServer,
        request_count: usize,
    ) -> (String, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let handle = thread::spawn(move || {
            listener
                .set_nonblocking(true)
                .expect("set test server nonblocking");
            let deadline = Instant::now() + Duration::from_secs(2);
            let mut requests = 0;
            let mut handlers = Vec::new();
            while requests < request_count && Instant::now() < deadline {
                let Ok((stream, _)) = listener.accept() else {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                };
                requests += 1;
                let response = response.clone();
                handlers.push(thread::spawn(move || handle_request(stream, response)));
            }
            for handler in handlers {
                handler.join().expect("test request handler");
            }
        });
        (format!("http://{address}/sample.mov"), handle)
    }

    fn handle_request(mut stream: TcpStream, response: TestServer) {
        stream
            .set_nonblocking(false)
            .expect("set test stream blocking");
        let request = read_request(&mut stream);
        let method = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().next())
            .unwrap_or_default();
        let body = if method == "HEAD" {
            Vec::new()
        } else {
            response.body.clone()
        };

        let active = response.probe.as_ref().filter(|_| method == "GET");
        let active_count = active.map(|probe| {
            let current = probe.active.fetch_add(1, Ordering::SeqCst) + 1;
            let mut maximum = probe.maximum.load(Ordering::SeqCst);
            while current > maximum {
                match probe.maximum.compare_exchange(
                    maximum,
                    current,
                    Ordering::SeqCst,
                    Ordering::SeqCst,
                ) {
                    Ok(_) => break,
                    Err(updated) => maximum = updated,
                }
            }
            current
        });
        if active_count.is_some()
            && let Some(probe) = response.probe.as_ref()
        {
            probe.gate.wait_for_pair();
        }

        let mut headers = format!("HTTP/1.1 {}\r\nConnection: close\r\n", response.status);
        if let Some(length) = response.head_length {
            headers.push_str(&format!("Content-Length: {length}\r\n"));
        }
        headers.push_str("\r\n");
        stream
            .write_all(headers.as_bytes())
            .and_then(|_| stream.write_all(&body))
            .expect("write test response");
        if active_count.is_some()
            && let Some(probe) = response.probe
        {
            probe.active.fetch_sub(1, Ordering::SeqCst);
        }
    }

    fn read_request(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set server timeout");
        let mut bytes = Vec::new();
        let mut buffer = [0u8; 1024];
        while !bytes.windows(4).any(|window| window == b"\r\n\r\n") {
            let count = stream.read(&mut buffer).expect("read test request");
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        String::from_utf8_lossy(&bytes).into_owned()
    }

    fn collect_events(rx: Receiver<DownloadEvent>) -> Vec<DownloadEvent> {
        let mut events = Vec::new();
        loop {
            let event = rx
                .recv_timeout(Duration::from_secs(5))
                .expect("download event");
            let complete = matches!(event, DownloadEvent::Complete);
            events.push(event);
            if complete {
                break;
            }
        }
        events
    }

    #[test]
    fn downloads_one_asset_through_observable_queue() {
        let body = (0..(128 * 1024 + 17))
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>();
        let (url, server) = spawn_server(
            TestServer::new(body.clone(), Some(body.len() as u64), "200 OK"),
            2,
        );
        let output_dir = temp_dir();
        let target = output_dir.join("sample.mov");
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset(url)],
                output_dir: output_dir.to_path_buf(),
                threads: 0,
            },
            tx,
        );

        let events = collect_events(rx);
        server.join().expect("test server");

        assert_eq!(fs::read(&target).expect("final target"), body);
        assert!(!output_dir.join("sample.mov.part").exists());
        assert!(matches!(
            events.first(),
            Some(DownloadEvent::Started { .. })
        ));
        let progress = events
            .iter()
            .filter_map(|event| match event {
                DownloadEvent::Progress {
                    bytes_downloaded,
                    total_bytes,
                    ..
                } => Some((*bytes_downloaded, *total_bytes)),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert!(progress.len() >= 2);
        assert!(progress.windows(2).all(|pair| pair[0].0 < pair[1].0));
        assert!(progress.iter().all(|(bytes, total)| {
            *total == Some(body.len() as u64) && *bytes <= body.len() as u64
        }));
        assert_eq!(
            progress.last().map(|(bytes, _)| *bytes),
            Some(body.len() as u64)
        );
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DownloadEvent::Finished { .. }))
        );
        assert!(matches!(events.last(), Some(DownloadEvent::Complete)));
    }

    #[test]
    fn replaces_existing_asset_when_remote_size_differs() {
        let body = b"new-wallpaper".to_vec();
        let (url, server) = spawn_server(
            TestServer::new(body.clone(), Some(body.len() as u64), "200 OK"),
            2,
        );
        let output_dir = temp_dir();
        let target = output_dir.join("sample.mov");
        fs::write(&target, b"stale").expect("write stale target");
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset(url)],
                output_dir: output_dir.to_path_buf(),
                threads: 1,
            },
            tx,
        );

        let events = collect_events(rx);
        server.join().expect("test server");

        assert_eq!(fs::read(&target).expect("replacement target"), body);
        assert!(!output_dir.join("sample.mov.part").exists());
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DownloadEvent::Finished { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, DownloadEvent::Skipped { .. }))
        );
    }

    #[test]
    fn skips_existing_asset_only_when_remote_size_matches() {
        let body = b"already-complete".to_vec();
        let (url, server) = spawn_server(
            TestServer::new(body.clone(), Some(body.len() as u64), "200 OK"),
            1,
        );
        let output_dir = temp_dir();
        let target = output_dir.join("sample.mov");
        fs::write(&target, &body).expect("write existing target");
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset(url)],
                output_dir: output_dir.to_path_buf(),
                threads: 1,
            },
            tx,
        );

        let events = collect_events(rx);
        server.join().expect("test server");

        assert!(matches!(
            events.as_slice(),
            [DownloadEvent::Skipped { .. }, DownloadEvent::Complete]
        ));
    }

    #[test]
    fn downloads_existing_asset_again_when_remote_size_is_unknown() {
        let body = b"fresh-unknown-size".to_vec();
        let (url, server) = spawn_server(TestServer::new(body.clone(), None, "200 OK"), 2);
        let output_dir = temp_dir();
        let target = output_dir.join("sample.mov");
        fs::write(&target, b"stale").expect("write stale target");
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset(url)],
                output_dir: output_dir.to_path_buf(),
                threads: 1,
            },
            tx,
        );

        let events = collect_events(rx);
        server.join().expect("test server");

        assert_eq!(fs::read(&target).expect("fresh target"), body);
        assert!(events.iter().any(|event| matches!(
            event,
            DownloadEvent::Started {
                total_bytes: None,
                ..
            }
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            DownloadEvent::Progress {
                bytes_downloaded,
                total_bytes: None,
                ..
            } if *bytes_downloaded > 0
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DownloadEvent::Finished { .. }))
        );
        assert!(!output_dir.join("sample.mov.part").exists());
    }

    #[test]
    fn failed_asset_emits_error_and_does_not_stop_other_assets() {
        let (failed_url, failed_server) = spawn_server(
            TestServer::new(Vec::new(), None, "500 Internal Server Error"),
            2,
        );
        let body = b"successful".to_vec();
        let (successful_url, successful_server) = spawn_server(
            TestServer::new(body.clone(), Some(body.len() as u64), "200 OK"),
            2,
        );
        let output_dir = temp_dir();
        let mut successful_asset = wallpaper_asset(successful_url);
        successful_asset.id = "successful".to_owned();
        successful_asset.file_name = "successful.mov".to_owned();
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset(failed_url), successful_asset],
                output_dir: output_dir.to_path_buf(),
                threads: 2,
            },
            tx,
        );

        let events = collect_events(rx);
        failed_server.join().expect("failed test server");
        successful_server.join().expect("successful test server");

        assert!(events.iter().any(|event| matches!(
            event,
            DownloadEvent::Error { index: 0, message } if message.contains("500")
        )));
        assert!(
            events
                .iter()
                .any(|event| matches!(event, DownloadEvent::Finished { index: 1, .. }))
        );
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, DownloadEvent::Complete))
                .count(),
            1
        );
        assert_eq!(
            fs::read(output_dir.join("successful.mov")).expect("successful target"),
            body
        );
    }

    #[test]
    fn interrupted_replacement_cleans_part_and_preserves_target() {
        let advertised_size = 32;
        let (url, server) = spawn_server(
            TestServer::new(b"short".to_vec(), Some(advertised_size), "200 OK"),
            2,
        );
        let output_dir = temp_dir();
        let target = output_dir.join("sample.mov");
        fs::write(&target, b"previous-complete-wallpaper").expect("write previous target");
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset(url)],
                output_dir: output_dir.to_path_buf(),
                threads: 1,
            },
            tx,
        );

        let events = collect_events(rx);
        server.join().expect("test server");

        assert!(
            events
                .iter()
                .any(|event| matches!(event, DownloadEvent::Error { index: 0, .. }))
        );
        assert_eq!(
            fs::read(&target).expect("preserved target"),
            b"previous-complete-wallpaper"
        );
        assert!(!output_dir.join("sample.mov.part").exists());
    }

    #[test]
    fn finalization_failure_cleans_part_without_destroying_target() {
        let body = b"replacement".to_vec();
        let (url, server) = spawn_server(
            TestServer::new(body.clone(), Some(body.len() as u64), "200 OK"),
            2,
        );
        let output_dir = temp_dir();
        let target = output_dir.join("sample.mov");
        fs::create_dir(&target).expect("create blocking target directory");
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset(url)],
                output_dir: output_dir.to_path_buf(),
                threads: 1,
            },
            tx,
        );

        let events = collect_events(rx);
        server.join().expect("test server");

        assert!(events.iter().any(|event| matches!(event, DownloadEvent::Error { index: 0, message } if message.contains("finalize"))));
        assert!(target.is_dir());
        assert!(!output_dir.join("sample.mov.part").exists());
    }

    #[test]
    fn filesystem_setup_failure_is_an_asset_error() {
        let root = temp_dir();
        let output_path = root.join("not-a-directory");
        fs::write(&output_path, b"file").expect("write blocking file");
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![wallpaper_asset("http://127.0.0.1:1/unreachable".to_owned())],
                output_dir: output_path,
                threads: 0,
            },
            tx,
        );

        let events = collect_events(rx);

        assert!(matches!(
            events.as_slice(),
            [
                DownloadEvent::Error { index: 0, .. },
                DownloadEvent::Complete
            ]
        ));
    }

    #[test]
    fn mixed_outcomes_have_one_terminal_event_and_one_final_completion() {
        let skip_body = b"skip".to_vec();
        let (skip_url, skip_server) = spawn_server(
            TestServer::new(skip_body.clone(), Some(skip_body.len() as u64), "200 OK"),
            1,
        );
        let (error_url, error_server) = spawn_server(
            TestServer::new(Vec::new(), None, "503 Service Unavailable"),
            2,
        );
        let success_body = b"done".to_vec();
        let (success_url, success_server) = spawn_server(
            TestServer::new(
                success_body.clone(),
                Some(success_body.len() as u64),
                "200 OK",
            ),
            2,
        );
        let output_dir = temp_dir();
        let mut skipped_asset = wallpaper_asset(skip_url);
        skipped_asset.file_name = "skipped.mov".to_owned();
        fs::write(output_dir.join("skipped.mov"), skip_body).expect("write skipped target");
        let mut error_asset = wallpaper_asset(error_url);
        error_asset.file_name = "error.mov".to_owned();
        let mut successful_asset = wallpaper_asset(success_url);
        successful_asset.file_name = "success.mov".to_owned();
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets: vec![skipped_asset, error_asset, successful_asset],
                output_dir: output_dir.to_path_buf(),
                threads: 3,
            },
            tx,
        );

        let events = collect_events(rx);
        skip_server.join().expect("skip test server");
        error_server.join().expect("error test server");
        success_server.join().expect("success test server");

        for index in 0..3 {
            let terminal_count = events
                .iter()
                .filter(|event| match event {
                    DownloadEvent::Finished {
                        index: event_index, ..
                    }
                    | DownloadEvent::Skipped {
                        index: event_index, ..
                    }
                    | DownloadEvent::Error {
                        index: event_index, ..
                    } => *event_index == index,
                    _ => false,
                })
                .count();
            assert_eq!(terminal_count, 1);
        }
        assert!(matches!(events.last(), Some(DownloadEvent::Complete)));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, DownloadEvent::Complete))
                .count(),
            1
        );
        assert_eq!(
            fs::read(output_dir.join("success.mov")).expect("successful target"),
            success_body
        );
    }

    #[test]
    fn worker_limit_and_completion_are_observable() {
        let probe = Arc::new(ConcurrencyProbe {
            active: AtomicUsize::new(0),
            maximum: AtomicUsize::new(0),
            gate: ConcurrencyGate::new(),
        });
        let body = b"x".to_vec();
        let (url, server) = spawn_server(
            TestServer::new(body.clone(), Some(1), "200 OK").with_probe(Arc::clone(&probe)),
            10,
        );
        let output_dir = temp_dir();
        let assets = (0..5)
            .map(|index| {
                let mut asset = wallpaper_asset(url.clone());
                asset.id = format!("asset-{index}");
                asset.file_name = format!("asset-{index}.mov");
                asset
            })
            .collect();
        let (tx, rx) = mpsc::channel();
        start_downloads(
            DownloadPlan {
                assets,
                output_dir: output_dir.to_path_buf(),
                threads: 2,
            },
            tx,
        );

        let events = collect_events(rx);
        server.join().expect("concurrency test server");

        assert!(probe.maximum.load(Ordering::SeqCst) <= 2);
        assert_eq!(probe.maximum.load(Ordering::SeqCst), 2);
        assert!(matches!(events.last(), Some(DownloadEvent::Complete)));
        assert_eq!(
            events
                .iter()
                .filter(|event| matches!(event, DownloadEvent::Complete))
                .count(),
            1
        );
        for index in 0..5 {
            let terminal_positions: Vec<_> = events
                .iter()
                .enumerate()
                .filter_map(|(position, event)| match event {
                    DownloadEvent::Finished {
                        index: event_index, ..
                    }
                    | DownloadEvent::Skipped {
                        index: event_index, ..
                    }
                    | DownloadEvent::Error {
                        index: event_index, ..
                    } if *event_index == index => Some(position),
                    _ => None,
                })
                .collect();
            assert_eq!(terminal_positions.len(), 1);
            assert!(terminal_positions[0] < events.len() - 1);
            assert!(!events[terminal_positions[0] + 1..].iter().any(|event| {
                matches!(event, DownloadEvent::Progress { index: event_index, .. } if *event_index == index)
            }));
        }
    }

    #[test]
    fn is_up_to_date_returns_false_when_no_expected_length() {
        let path = std::env::temp_dir().join("awm_uptodate_none_test.mov");
        assert!(!is_up_to_date(&path, None));
    }

    #[test]
    fn is_up_to_date_returns_false_when_file_missing() {
        let path = std::env::temp_dir().join("awm_uptodate_missing_test_xyz.mov");
        let _ = fs::remove_file(&path);
        assert!(!is_up_to_date(&path, Some(100)));
    }

    #[test]
    fn is_up_to_date_returns_true_when_size_matches() {
        let temp_dir =
            std::env::temp_dir().join(format!("awm-uptodate-test-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();
        let path = temp_dir.join("test.mov");
        let content = b"hello world";
        fs::write(&path, content).unwrap();
        assert!(is_up_to_date(&path, Some(content.len() as u64)));
        assert!(!is_up_to_date(&path, Some(999)));
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn download_stream_emits_progress_events() {
        let temp_dir =
            std::env::temp_dir().join(format!("awm-progress-test-{}", std::process::id()));
        fs::create_dir_all(&temp_dir).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        let input = b"hello progress world".to_vec();
        let mut cursor = Cursor::new(input.clone());
        let temp_path = temp_dir.join("test.part");
        download_stream(&mut cursor, &temp_path, Some(input.len() as u64), 0, &tx).unwrap();
        let events: Vec<_> = rx.try_iter().collect();
        let has_progress = events
            .iter()
            .any(|e| matches!(e, DownloadEvent::Progress { .. }));
        assert!(has_progress);
        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn streams_bytes_to_disk() {
        let temp_dir = temp_dir();
        let (tx, _rx) = std::sync::mpsc::channel();
        let input = b"wallpaper-bytes".to_vec();
        let mut cursor = Cursor::new(input.clone());
        let temp_path = temp_dir.join("sample.mov.part");
        download_stream(&mut cursor, &temp_path, Some(input.len() as u64), 0, &tx)
            .expect("stream bytes");
        let downloaded = fs::read(&temp_path).expect("downloaded file");
        assert_eq!(downloaded, input);
    }
}
