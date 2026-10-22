// Protocol tests for overlapping claim bursts. The server can hold early
// response bodies without holding the later /api/next endpoint.
struct DelayedDownloadHopper {
    shared: Arc<WorkerShared>,
    rx: mpsc::UnboundedReceiver<PrefetchedJob>,
    prefetcher: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<()>,
    release: Arc<Semaphore>,
    held_started: Arc<AtomicUsize>,
    peak_downloads: Arc<AtomicUsize>,
}

impl Drop for DelayedDownloadHopper {
    fn drop(&mut self) {
        self.shared.stop.raise(Exit::Finished);
        self.prefetcher.abort();
        self.server.abort();
    }
}

struct ActiveTestDownload(Arc<AtomicUsize>);
impl Drop for ActiveTestDownload {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

impl DelayedDownloadHopper {
    async fn start(held_count: usize, total: usize, depth: usize) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let release = Arc::new(Semaphore::new(0));
        let held_started = Arc::new(AtomicUsize::new(0));
        let active = Arc::new(AtomicUsize::new(0));
        let peak_downloads = Arc::new(AtomicUsize::new(0));
        let next = Arc::new(AtomicUsize::new(0));
        let payloads: Arc<Vec<_>> = Arc::new(
            (0..total)
                .map(|i| {
                    let bytes = format!("payload-{i}").into_bytes();
                    (sha256_hex(&bytes), bytes)
                })
                .collect(),
        );
        let server = {
            let release = Arc::clone(&release);
            let held_started = Arc::clone(&held_started);
            let peak = Arc::clone(&peak_downloads);
            tokio::spawn(async move {
                let mut requests = JoinSet::new();
                loop {
                    let Ok((mut stream, _)) = listener.accept().await else {
                        break;
                    };
                    while requests.try_join_next().is_some() {}
                    let release = Arc::clone(&release);
                    let held_started = Arc::clone(&held_started);
                    let active = Arc::clone(&active);
                    let peak = Arc::clone(&peak);
                    let next = Arc::clone(&next);
                    let payloads = Arc::clone(&payloads);
                    requests.spawn(async move {
                        let Some(target) = read_target(&mut stream).await else {
                            return;
                        };
                        if target.starts_with("/api/next") {
                            let count = parse_count(&target);
                            let start = next.fetch_add(count, Ordering::AcqRel);
                            // Make the overlap/concurrency checks independent
                            // of task launch order: the later claim may return
                            // while early bodies wait, once their requests began.
                            if start >= 2 {
                                while held_started.load(Ordering::Acquire) < held_count {
                                    tokio::time::sleep(Duration::from_millis(1)).await;
                                }
                            }
                            if start >= payloads.len() {
                                respond(&mut stream, "204 No Content", b"").await;
                                return;
                            }
                            let jobs: Vec<_> = (start..(start+count).min(payloads.len())).map(|id| {
                                let (sha, bytes) = &payloads[id];
                                serde_json::json!({"sha256":sha,"path":format!("samples/{id}.bin"),
                                    "size_bytes":bytes.len(),"file_type":"data"})
                            }).collect();
                            let body = serde_json::json!({"jobs":jobs}).to_string();
                            respond(&mut stream, "200 OK", body.as_bytes()).await;
                        } else if target.starts_with("/data/") {
                            let id: usize = target
                                .strip_prefix("/data/samples/")
                                .and_then(|path| path.strip_suffix(".bin"))
                                .expect("fixture data path")
                                .parse()
                                .expect("fixture numeric member");
                            let now = active.fetch_add(1, Ordering::AcqRel) + 1;
                            let _active = ActiveTestDownload(active);
                            peak.fetch_max(now, Ordering::AcqRel);
                            if id < held_count {
                                held_started.fetch_add(1, Ordering::AcqRel);
                                let _release = release.acquire().await.unwrap();
                                respond(&mut stream, "200 OK", &payloads[id].1).await;
                            } else {
                                respond(&mut stream, "200 OK", &payloads[id].1).await;
                            }
                        } else {
                            respond(&mut stream, "404 Not Found", b"").await;
                        }
                    });
                }
            })
        };
        let (tx, rx) = mpsc::unbounded_channel();
        let mut config = test_config(&format!("http://127.0.0.1:{port}"));
        config.workers = NonZeroUsize::new(2).unwrap();
        let shared = test_shared(&config, |shared| {
            shared.target_depth = depth;
            shared.spool = test_spool(1024);
            shared.input_budget = InputBudget::new(1024);
            shared.hopper.input_budget = Some(Arc::clone(&shared.input_budget));
        });
        let prefetcher = tokio::spawn(prefetch_loop(Arc::clone(&shared), tx));
        Self {
            shared,
            rx,
            prefetcher,
            server,
            release,
            held_started,
            peak_downloads,
        }
    }

    async fn stop_and_verify_released(&mut self) {
        self.shared.stop.raise(Exit::Finished);
        tokio::time::timeout(Duration::from_secs(2), &mut self.prefetcher)
            .await
            .expect("stopping prefetch must cancel held downloads")
            .expect("prefetch task");
        tokio::time::timeout(Duration::from_secs(2), async {
            while self.rx.recv().await.is_some() {}
        })
        .await
        .expect("all staging senders must close on stop");
        assert!(
            wait_until(|| self.shared.input_budget.used.load(Ordering::Acquire) == 0).await,
            "cancelling a burst leaked input reservations"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetcher_polls_while_an_earlier_download_waits() {
    let mut hopper = DelayedDownloadHopper::start(1, 3, 3).await;
    let first = tokio::time::timeout(Duration::from_secs(2), hopper.rx.recv())
        .await
        .unwrap()
        .unwrap();
    let mut paths = vec![first.job.path.clone()];
    drop(first);
    let later = tokio::time::timeout(Duration::from_secs(2), hopper.rx.recv()).await;
    assert!(later.is_ok(), "one slow response held back the next claim");
    let later = later.unwrap().unwrap();
    paths.push(later.job.path.clone());
    drop(later);
    paths.sort();
    assert_eq!(paths, ["samples/1.bin", "samples/2.bin"]);
    assert_eq!(hopper.shared.outstanding.load(Ordering::Acquire), 3);
    assert!(hopper.peak_downloads.load(Ordering::Acquire) <= 2);
    hopper.release.add_permits(1);
    let held = tokio::time::timeout(Duration::from_secs(2), hopper.rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(held.job.path, "samples/0.bin");
    drop(held);
    hopper.stop_and_verify_released().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetcher_limits_downloads_across_overlapping_claims() {
    let mut hopper = DelayedDownloadHopper::start(2, 4, 4).await;
    assert!(
        wait_until(|| hopper.held_started.load(Ordering::Acquire) == 2
            && hopper.shared.outstanding.load(Ordering::Acquire) == 4)
        .await,
        "later claims must fill queue capacity while earlier bodies wait"
    );
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(
        hopper.peak_downloads.load(Ordering::Acquire),
        2,
        "overlapping claims exceeded the existing active-download bound"
    );
    assert!(
        hopper.rx.try_recv().is_err(),
        "a third body started while both download permits were held"
    );
    assert_eq!(hopper.shared.outstanding.load(Ordering::Acquire), 4);
    hopper.release.add_permits(2);
    let mut paths = Vec::new();
    for _ in 0..4 {
        let job = tokio::time::timeout(Duration::from_secs(2), hopper.rx.recv())
            .await
            .unwrap()
            .unwrap();
        paths.push(job.job.path.clone());
        drop(job);
    }
    paths.sort();
    assert_eq!(
        paths,
        [
            "samples/0.bin",
            "samples/1.bin",
            "samples/2.bin",
            "samples/3.bin"
        ]
    );
    hopper.stop_and_verify_released().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetcher_stop_cancels_downloads_from_every_claim() {
    let mut hopper = DelayedDownloadHopper::start(2, 4, 4).await;
    assert!(
        wait_until(|| hopper.held_started.load(Ordering::Acquire) == 2
            && hopper.shared.outstanding.load(Ordering::Acquire) == 4)
        .await
    );
    // Leave both HTTP bodies held and later requests waiting for download permits.
    hopper.stop_and_verify_released().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn prefetcher_exits_when_the_consumer_closes_during_overlapping_claims() {
    let mut hopper = DelayedDownloadHopper::start(2, 4, 4).await;
    assert!(
        wait_until(|| hopper.held_started.load(Ordering::Acquire) == 2
            && hopper.shared.outstanding.load(Ordering::Acquire) == 4)
        .await
    );
    hopper.rx.close();
    hopper.release.add_permits(2);
    tokio::time::timeout(Duration::from_secs(2), &mut hopper.prefetcher)
        .await
        .expect("closed consumer must wake a producer waiting for queue room")
        .expect("prefetch task");
    assert!(
        wait_until(|| hopper.shared.input_budget.used.load(Ordering::Acquire) == 0).await,
        "closed consumer leaked staged bytes"
    );
}
