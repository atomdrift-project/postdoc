//! Deterministic allocation fixture for the public post-fetch graft path.
//! Arguments: rules directory, member count, evidence bytes/member, declaring
//! member count, optional full-report output path. Build both variants with
//! the same profile and run each in a fresh process; compare the full reports.

use cleave::types::{Evidence, Finding};
use cleave::{AnalysisOptions, AnalysisReport, FileAnalysis, TargetInfo};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;
use std::collections::BTreeMap;
use std::time::Instant;

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<Option<(u64, u64)>> = const { Cell::new(None) };
}

fn record_allocation(size: usize) {
    let _ = ALLOCATIONS.try_with(|counter| {
        if let Some((bytes, calls)) = counter.get() {
            counter.set(Some((bytes + size as u64, calls + 1)));
        }
    });
}

// Count cumulative allocation volume on the measured thread. The report seed
// and rule-loader warmup are constructed before enabling the counter. Frees
// deliberately do not subtract: this measures copying/work, not live memory.
#[allow(unsafe_code)] // This fixture delegates each allocator operation to System.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record_allocation(size);
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn finding(name: &str) -> Finding {
    Finding::capability(
        format!("micro-behaviors/process/create/task::{name}"),
        name.to_string(),
        1.0,
    )
}

#[allow(clippy::expect_used)] // Fixture IDs must fit the wire format.
fn report(members: usize, evidence_bytes: usize, declaring: usize) -> AnalysisReport {
    let mut report = AnalysisReport::new(TargetInfo {
        path: "fixture.zip".into(),
        file_type: "zip".into(),
        size_bytes: 1,
        sha256: "root".into(),
        architectures: None,
    });
    // Fix the input timestamp before evaluation so complete output equality
    // remains meaningful across fresh processes.
    report.analysis_timestamp = None;
    report.files.push(FileAnalysis {
        id: 0,
        path: "fixture.zip".into(),
        file_type: "zip".into(),
        sha256: "root".into(),
        ..Default::default()
    });
    for i in 0..members {
        let mut bulk = finding("bulk");
        bulk.evidence
            .push(Evidence::new("text", "fixture", "x".repeat(evidence_bytes)));
        report.files.push(FileAnalysis {
            id: u32::try_from(i + 1).expect("fixture id fits u32"),
            parent_id: Some(0),
            path: format!("fixture.zip!!member-{i}.js"),
            file_type: "javascript".into(),
            sha256: format!("source-{i}"),
            depth: 1,
            findings: vec![finding("command"), finding("startup"), bulk],
            filefacts_metrics: (i < declaring).then(|| {
                BTreeMap::from([
                    ("references.declared_count".into(), 1.0),
                    ("references.unavailable_command_count".into(), 1.0),
                ])
            }),
            ..Default::default()
        });
    }
    report
}

fn proc_kib(field: &str) -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|status| {
            status.lines().find_map(|line| {
                line.strip_prefix(field)?
                    .split_whitespace()
                    .next()?
                    .parse()
                    .ok()
            })
        })
        .unwrap_or(0)
}

#[allow(clippy::expect_used)] // A missing enabled allocation counter is a fixture bug.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut args = std::env::args().skip(1);
    let rules = args.next().ok_or("rules directory required")?;
    let members = args
        .next()
        .unwrap_or_else(|| "512".into())
        .parse::<usize>()?;
    let evidence = args
        .next()
        .unwrap_or_else(|| "65536".into())
        .parse::<usize>()?;
    let declaring = args
        .next()
        .unwrap_or_else(|| "8".into())
        .parse::<usize>()?
        .min(members);
    let output = args.next();
    if members == 0 || declaring == 0 {
        return Err("members and declaring must be positive".into());
    }
    cleave::traits_repo::set_override_dir(Some(rules.into()));
    let options = AnalysisOptions::default();
    let mut warmup = report(2, 0, 1);
    cleave::graft_reference_outcome_traits(&mut warmup, "source-0", &options)?;
    drop(warmup);
    let mut report = report(members, evidence, declaring);
    let seed_rss = proc_kib("VmRSS:");
    ALLOCATIONS.with(|counter| counter.set(Some((0, 0))));
    let started = Instant::now();
    let mut added = 0;
    for i in 0..declaring {
        added +=
            cleave::graft_reference_outcome_traits(&mut report, &format!("source-{i}"), &options)?;
    }
    let elapsed = started.elapsed();
    let (allocated, calls) =
        ALLOCATIONS.with(|counter| counter.replace(None).expect("counter active"));
    let peak_rss = proc_kib("VmHWM:");
    assert!(
        report.files[0]
            .findings
            .iter()
            .any(|f| f.id.ends_with("::archive-chain"))
    );
    for file in report.files.iter().skip(1).take(declaring) {
        assert!(file.findings.iter().any(|f| f.id.ends_with("::local")));
    }
    println!(
        "{{\"members\":{members},\"evidence_bytes\":{evidence},\"declaring\":{declaring},\"added\":{added},\"allocated_bytes\":{allocated},\"allocation_calls\":{calls},\"elapsed_ms\":{},\"seed_rss_kib\":{seed_rss},\"peak_rss_kib\":{peak_rss}}}",
        elapsed.as_millis()
    );
    if let Some(output) = output {
        std::fs::write(output, serde_json::to_vec(&report)?)?;
    }
    Ok(())
}
