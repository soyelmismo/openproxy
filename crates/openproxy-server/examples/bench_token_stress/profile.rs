//! Optional hardware counter collection, scoped by perf's control FIFO.
use anyhow::{Context, ensure};
use serde_json::{Value, json};
use std::{
    fs::{File, OpenOptions},
    io::{BufRead, BufReader, Write},
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    sync::mpsc,
    time::Duration,
};

pub struct Perf {
    child: Child,
    control: File,
    ack: mpsc::Receiver<std::io::Result<String>>,
    ack_writer: File,
    ack_task: Option<std::thread::JoinHandle<()>>,
    path: PathBuf,
}
impl Perf {
    pub fn start(pid: u32, directory: &Path, fsync: bool) -> anyhow::Result<Self> {
        let control_path = directory.join("perf-control");
        let ack_path = directory.join("perf-ack");
        let status = Command::new("mkfifo")
            .arg(&control_path)
            .arg(&ack_path)
            .status()?;
        ensure!(status.success(), "mkfifo failed");
        let control = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&control_path)?;
        let ack_file = OpenOptions::new().read(true).write(true).open(&ack_path)?;
        let ack_writer = ack_file.try_clone()?;
        let path = directory.join("perf-stat.csv");
        let events = if fsync {
            "cycles,instructions,cache-misses,task-clock,context-switches,page-faults,syscalls:sys_enter_fsync,syscalls:sys_enter_fdatasync"
        } else {
            "cycles,instructions,cache-misses,task-clock,context-switches,page-faults"
        };
        let child = Command::new("perf")
            .args([
                "stat",
                "-x;",
                "-D",
                "-1",
                "-e",
                events,
                "-p",
                &pid.to_string(),
                "--control",
                &format!("fifo:{},{}", control_path.display(), ack_path.display()),
                "-o",
            ])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(File::create(directory.join("perf.stderr"))?))
            .spawn()
            .context("perf not available")?;
        let (tx, rx) = mpsc::channel();
        let ack_task = std::thread::spawn(move || {
            for line in BufReader::new(ack_file).lines() {
                if line.as_ref().is_ok_and(|s| s == "stop") || tx.send(line).is_err() {
                    break;
                }
            }
        });
        let mut perf = Self {
            child,
            control,
            ack: rx,
            ack_writer,
            ack_task: Some(ack_task),
            path,
        };
        perf.command("enable")?;
        Ok(perf)
    }
    fn command(&mut self, command: &str) -> anyhow::Result<()> {
        writeln!(self.control, "{command}")?;
        self.control.flush()?;
        let ack = self
            .ack
            .recv_timeout(Duration::from_secs(5))
            .context("perf did not acknowledge; inspect perf.stderr (permissions/counters)")??;
        ensure!(
            ack.trim_matches(|c: char| c == '\0' || c.is_whitespace()) == "ack",
            "unexpected perf acknowledgement: {ack}"
        );
        Ok(())
    }
    pub fn stop(mut self) -> anyhow::Result<Value> {
        self.command("disable")?;
        Command::new("kill")
            .args(["-INT", &self.child.id().to_string()])
            .status()?;
        let _ = self.child.wait()?;
        let text = std::fs::read_to_string(&self.path)?;
        let mut counters = serde_json::Map::new();
        for line in text.lines().filter(|l| !l.starts_with('#')) {
            let columns: Vec<_> = line.split(';').collect();
            if let (Some(value), Some(unit), Some(event)) =
                (columns.first(), columns.get(1), columns.get(2))
            {
                counters.insert((*event).into(),json!({"value":value.trim().parse::<f64>().ok(),"raw":value.trim(),"unit":unit,"runtime_and_scaling":columns.get(3..)}));
            }
        }
        Ok(
            json!({"status":"collected", "scope":"proxy process/threads; enabled before go, disabled after persistence drain", "counters":counters, "raw_file":self.path}),
        )
    }
}
pub fn interpret_heaptrack(directory: &Path) -> anyhow::Result<Value> {
    let raw = directory.join("heaptrack-proxy.raw");
    let interpreted = directory.join("heaptrack-proxy.interpreted");
    let interpreter = std::env::var("HEAPTRACK_INTERPRET")
        .unwrap_or_else(|_| "/usr/lib/heaptrack/libexec/heaptrack_interpret".into());
    let status = Command::new(interpreter)
        .stdin(Stdio::from(File::open(&raw)?))
        .stdout(Stdio::from(File::create(&interpreted)?))
        .stderr(Stdio::from(File::create(
            directory.join("heaptrack-interpret.stderr"),
        )?))
        .status()?;
    ensure!(status.success(), "heaptrack interpreter failed");
    let summary = directory.join("heaptrack-summary.txt");
    let status = Command::new("heaptrack_print")
        .arg(&interpreted)
        .args(["-n", "20", "-H"])
        .arg(directory.join("allocation-sizes.tsv"))
        .arg("-F")
        .arg(directory.join("allocation-stacks.txt"))
        .stdout(Stdio::from(File::create(&summary)?))
        .status()?;
    ensure!(status.success(), "heaptrack_print failed");
    let text = std::fs::read_to_string(&summary)?;
    ensure!(
        text.lines()
            .any(|line| line.starts_with("Debuggee command was:") && line.contains("--role proxy")),
        "heaptrack did not profile the proxy executable"
    );
    Ok(
        json!({"status":"collected", "scope":"whole proxy lifetime incl setup/warmup; NOT per-request allocations", "allocator":"system; intrusive profile, do not compare throughput/RSS with mimalloc", "raw":raw, "interpreted":interpreted, "summary":summary, "size_histogram":directory.join("allocation-sizes.tsv"), "stacks":directory.join("allocation-stacks.txt")}),
    )
}

impl Drop for Perf {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        // O_RDWR keeps the FIFO open, so EOF cannot wake the reader. Send a
        // private sentinel after perf has exited, then join on every path.
        let _ = self.ack_writer.write_all(b"\nstop\n");
        if let Some(task) = self.ack_task.take() {
            let _ = task.join();
        }
    }
}
