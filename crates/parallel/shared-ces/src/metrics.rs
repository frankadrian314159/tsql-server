//! Resource metrics of a node: CPU, memory, disk and network, sampled in the background, and a plain-text export.
//!
//! A [`Sampler`] runs a thread that reads the operating system's counters every second (processor use of the machine and of
//! this process, memory, the process's disk reads and writes, and the bytes received and sent on all network interfaces) and
//! keeps the latest rates. Shard nodes and the control node each run one; the control node asks every shard for its own
//! (action `metrics`), and the autoscaler (see the control crate) decides from them where the bottleneck is.
//!
//! [`render_prometheus`] writes them in the Prometheus text format, and [`serve_http`] is a tiny HTTP listener for `/metrics`
//! and `/health`, so that Google Cloud's Ops Agent (or any Prometheus) can collect them.
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// One reading. Percentages are of the whole machine (all cores); rates are per second over the last sampling interval.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Sample {
    pub cores: usize,
    /// processor use of the whole machine, 0..100
    pub cpu_machine_pct: f64,
    /// processor use of this process, 0..100 of the whole machine
    pub cpu_process_pct: f64,
    pub mem_total_bytes: u64,
    pub mem_available_bytes: u64,
    /// resident memory of this process
    pub mem_process_bytes: u64,
    /// bytes this process read from and wrote to disk per second
    pub disk_read_bps: f64,
    pub disk_write_bps: f64,
    /// bytes received and sent per second on all network interfaces
    pub net_rx_bps: f64,
    pub net_tx_bps: f64,
}

impl Sample {
    /// Memory in use on the machine, 0..100.
    pub fn mem_used_pct(&self) -> f64 {
        if self.mem_total_bytes == 0 {
            0.0
        } else {
            100.0 * (self.mem_total_bytes - self.mem_available_bytes.min(self.mem_total_bytes)) as f64 / self.mem_total_bytes as f64
        }
    }
    pub fn disk_bps(&self) -> f64 {
        self.disk_read_bps + self.disk_write_bps
    }
    pub fn net_bps(&self) -> f64 {
        self.net_rx_bps + self.net_tx_bps
    }
}

/// What a shard reports to the control node: the resource sample and the engine's own counters.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeMetrics {
    pub sample: Sample,
    pub sessions: usize,
    /// statements being run right now, and run since the node started
    pub in_flight: u64,
    pub statements: u64,
    /// lock waits and the seconds waited in all, and log forces (the engine's measure of lock contention and commit rate)
    pub lock_waits: u64,
    pub lock_wait_secs: f64,
    pub log_forces: u64,
    /// the node is draining for a shutdown: it takes no new work
    pub draining: bool,
    pub uptime_secs: u64,
}

/// A background sampler of the machine's counters.
#[derive(Clone)]
pub struct Sampler {
    latest: Arc<Mutex<Sample>>,
}

impl Sampler {
    /// Start sampling every `every`. The first reading is made before this returns.
    pub fn start(every: Duration) -> Sampler {
        use sysinfo::{Networks, Pid, System};
        let latest = Arc::new(Mutex::new(Sample::default()));
        let out = latest.clone();
        let (ready_tx, ready_rx) = std::sync::mpsc::channel::<()>();
        std::thread::Builder::new()
            .name("tsql-metrics".into())
            .spawn(move || {
                let mut sys = System::new();
                let mut nets = Networks::new_with_refreshed_list();
                let pid = Pid::from_u32(std::process::id());
                let cores = {
                    sys.refresh_cpu();
                    sys.cpus().len().max(1)
                };
                // (the counters are deltas since the last refresh and a processor reading needs two: a first refresh sets the
                // baseline, so that the first published reading is a true one)
                sys.refresh_process(pid);
                nets.refresh();
                std::thread::sleep(Duration::from_millis(250));
                let mut last = Instant::now() - Duration::from_millis(250);
                let mut first = true;
                loop {
                    // (the sampler ends when every handle to it is gone)
                    if Arc::strong_count(&out) == 1 {
                        break;
                    }
                    sys.refresh_cpu();
                    sys.refresh_memory();
                    sys.refresh_process(pid);
                    nets.refresh();
                    let dt = last.elapsed().as_secs_f64().max(0.001);
                    last = Instant::now();
                    let (mut rx, mut tx) = (0u64, 0u64);
                    for (_, n) in nets.iter() {
                        rx += n.received();
                        tx += n.transmitted();
                    }
                    let (mut cpu_p, mut mem_p, mut rd, mut wr) = (0.0, 0, 0u64, 0u64);
                    if let Some(p) = sys.process(pid) {
                        // (a process's use is in percent of one core)
                        cpu_p = p.cpu_usage() as f64 / cores as f64;
                        mem_p = p.memory();
                        let d = p.disk_usage();
                        rd = d.read_bytes;
                        wr = d.written_bytes;
                    }
                    let s = Sample {
                        cores,
                        cpu_machine_pct: sys.global_cpu_info().cpu_usage() as f64,
                        cpu_process_pct: cpu_p.min(100.0),
                        mem_total_bytes: sys.total_memory(),
                        mem_available_bytes: sys.available_memory(),
                        mem_process_bytes: mem_p,
                        disk_read_bps: rd as f64 / dt,
                        disk_write_bps: wr as f64 / dt,
                        net_rx_bps: rx as f64 / dt,
                        net_tx_bps: tx as f64 / dt,
                    };
                    *out.lock().unwrap_or_else(|e| e.into_inner()) = s;
                    if first {
                        first = false;
                        let _ = ready_tx.send(());
                    }
                    std::thread::sleep(every);
                }
            })
            .ok();
        let _ = ready_rx.recv_timeout(Duration::from_secs(2));
        Sampler { latest }
    }

    pub fn sample(&self) -> Sample {
        self.latest.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// The metrics of nodes in the Prometheus text format; `nodes` are (label, metrics) pairs (`node="shard-0"`).
pub fn render_prometheus(nodes: &[(String, NodeMetrics)]) -> String {
    let mut out = String::new();
    let mut gauge = |name: &str, help: &str, f: &dyn Fn(&NodeMetrics) -> f64| {
        out.push_str(&format!("# HELP tsql_{} {}\n# TYPE tsql_{} gauge\n", name, help, name));
        for (label, m) in nodes {
            out.push_str(&format!("tsql_{}{{node=\"{}\"}} {}\n", name, label.replace('"', ""), f(m)));
        }
    };
    gauge("cpu_machine_percent", "processor use of the machine", &|m| m.sample.cpu_machine_pct);
    gauge("cpu_process_percent", "processor use of the node process, percent of the machine", &|m| m.sample.cpu_process_pct);
    gauge("memory_used_percent", "memory in use on the machine", &|m| m.sample.mem_used_pct());
    gauge("memory_process_bytes", "resident memory of the node process", &|m| m.sample.mem_process_bytes as f64);
    gauge("disk_read_bytes_per_second", "bytes the process reads from disk", &|m| m.sample.disk_read_bps);
    gauge("disk_write_bytes_per_second", "bytes the process writes to disk", &|m| m.sample.disk_write_bps);
    gauge("network_receive_bytes_per_second", "bytes received on all interfaces", &|m| m.sample.net_rx_bps);
    gauge("network_transmit_bytes_per_second", "bytes sent on all interfaces", &|m| m.sample.net_tx_bps);
    gauge("sessions", "open sessions", &|m| m.sessions as f64);
    gauge("statements_in_flight", "statements running now", &|m| m.in_flight as f64);
    gauge("statements_total", "statements run since the node started", &|m| m.statements as f64);
    gauge("lock_waits_total", "lock waits since the node started", &|m| m.lock_waits as f64);
    gauge("lock_wait_seconds_total", "seconds spent waiting for locks", &|m| m.lock_wait_secs);
    gauge("log_forces_total", "write-ahead log forces", &|m| m.log_forces as f64);
    gauge("draining", "1 while the node drains for a shutdown", &|m| if m.draining { 1.0 } else { 0.0 });
    gauge("uptime_seconds", "seconds since the node started", &|m| m.uptime_secs as f64);
    out
}

/// Serve `GET /metrics` (the text `render` returns) and `GET /health` (`ok`) on `addr`, on a background thread. Returns the
/// address it listens on (port 0 picks a free one).
pub fn serve_http(addr: &str, render: impl Fn() -> String + Send + Sync + 'static) -> std::io::Result<std::net::SocketAddr> {
    use std::io::{BufRead, BufReader, Write};
    let listener = std::net::TcpListener::bind(addr)?;
    let local = listener.local_addr()?;
    std::thread::Builder::new()
        .name("tsql-metrics-http".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let mut line = String::new();
                if BufReader::new(&stream).read_line(&mut line).is_err() {
                    continue;
                }
                let path = line.split_whitespace().nth(1).unwrap_or("/");
                let (status, body) = match path {
                    "/metrics" => ("200 OK", render()),
                    "/health" => ("200 OK", "ok\n".to_string()),
                    _ => ("404 Not Found", "not found\n".to_string()),
                };
                let _ = write!(stream, "HTTP/1.1 {}\r\nContent-Type: text/plain; version=0.0.4\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", status, body.len(), body);
            }
        })?;
    Ok(local)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_sampler_reads_the_machine() {
        let s = Sampler::start(Duration::from_millis(100));
        std::thread::sleep(Duration::from_millis(350));
        let x = s.sample();
        assert!(x.cores >= 1);
        assert!(x.mem_total_bytes > 0 && x.mem_process_bytes > 0, "{:?}", x);
        assert!(x.cpu_machine_pct >= 0.0 && x.cpu_machine_pct <= 100.0);
        assert!(x.mem_used_pct() > 0.0 && x.mem_used_pct() <= 100.0);
    }

    #[test]
    fn metrics_are_exported_as_text_over_http() {
        let m = NodeMetrics { sessions: 3, ..Default::default() };
        let nodes = vec![("shard-0".to_string(), m)];
        assert!(render_prometheus(&nodes).contains("tsql_sessions{node=\"shard-0\"} 3"));
        let addr = serve_http("127.0.0.1:0", move || render_prometheus(&nodes)).unwrap();
        use std::io::{Read, Write};
        let get = |path: &str| {
            let mut c = std::net::TcpStream::connect(addr).unwrap();
            write!(c, "GET {} HTTP/1.1\r\nHost: x\r\n\r\n", path).unwrap();
            let mut s = String::new();
            c.read_to_string(&mut s).unwrap();
            s
        };
        assert!(get("/metrics").contains("tsql_sessions{node=\"shard-0\"} 3"));
        assert!(get("/health").contains("ok"));
        assert!(get("/nope").starts_with("HTTP/1.1 404"));
    }
}
