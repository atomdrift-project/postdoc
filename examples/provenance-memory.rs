//! Measure retention and upload costs for cached registry JSON fixtures.
//! Run with --features bench-hopper; arguments: fixture directory, repetitions.

#[cfg(target_os = "linux")]
#[global_allocator]
static ALLOCATOR: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

fn rss_kib() -> anyhow::Result<u64> {
    let status = std::fs::read_to_string("/proc/self/status")?;
    Ok(status
        .lines()
        .find_map(|line| {
            line.strip_prefix("VmRSS:")?
                .split_whitespace()
                .next()?
                .parse()
                .ok()
        })
        .unwrap_or(0))
}

fn main() -> anyhow::Result<()> {
    use sha2::{Digest, Sha256};
    use std::time::Instant;
    let mut args = std::env::args().skip(1);
    let directory = args
        .next()
        .ok_or_else(|| anyhow::anyhow!("fixture directory required"))?;
    let repetitions: usize = args.next().unwrap_or_else(|| "1".to_string()).parse()?;
    let mut fixtures = std::fs::read_dir(directory)?
        .map(|entry| entry.map(|entry| entry.path()))
        .collect::<std::io::Result<Vec<_>>>()?;
    fixtures.retain(|path| path.extension().is_some_and(|ext| ext == "zst"));
    fixtures.sort();
    anyhow::ensure!(!fixtures.is_empty(), "no .zst fixtures");
    let started = Instant::now();
    let mut inputs = 0usize;
    let mut documents = Vec::new();
    for _ in 0..repetitions {
        for path in &fixtures {
            let bytes = zstd::stream::decode_all(std::fs::File::open(path)?)?;
            inputs += bytes.len();
            let source = fletch::fetch::RecordedSource {
                url: "https://registry.example/fixture".to_string(),
                status: 200,
                content_type: Some("application/json".to_string()),
                size: bytes.len() as u64,
                bytes: Some(bytes),
            };
            documents.push(scan::provenance::RegistryProvenance::from_record_sources(
                fletch::Registry {
                    ecosystem: "npm".to_string(),
                    name: "fixture".to_string(),
                    version: "1.0.0".to_string(),
                    ..Default::default()
                },
                &[source],
            ));
        }
    }
    let construct_ms = started.elapsed().as_millis();
    let retained_rss_kib = rss_kib()?;
    let started = Instant::now();
    let upload = scan::provenance::Upload {
        filename: "fixture.tgz",
        sha256: &"a".repeat(64),
        size_bytes: 1,
        collector: "benchmark",
        at: "2026-10-19T00:00:00Z",
        url: "",
        purl: Some("pkg:npm/fixture@1.0.0"),
    };
    let mut digest = Sha256::new();
    for document in &documents {
        digest.update(upload.sidecar_from_provenance(document)?);
    }
    let upload_ms = started.elapsed().as_millis();
    println!(
        "{}",
        serde_json::json!({
            "documents": documents.len(), "input_bytes": inputs,
            "construct_ms": construct_ms, "upload_ms": upload_ms,
            "total_ms": construct_ms + upload_ms, "retained_rss_kib": retained_rss_kib,
            "upload_sha256": format!("{:x}", digest.finalize()),
        })
    );
    Ok(())
}
