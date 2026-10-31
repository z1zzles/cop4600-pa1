// Implements FCFS, preemptive SJF, and Round Robin by driving real child
// processes (via process_control.rs) with a 100ms (one decisecond) tick
// loop paced against a real monotonic clock.

mod process_control;

use nix::unistd::Pid;
use process_control::{now_ms, preempt, reap_blocking, resume, spawn_paused, terminate_and_reap};
use std::env;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::process;
use std::thread;
use std::time::{Duration, Instant};

const TICK_MS: u64 = 100;

fn workload_path() -> String {
    env::var("WORKLOAD_PATH").unwrap_or_else(|_| "./workload".to_string())
}

#[derive(Debug, Clone)]
struct ProcSpec {
    name: String,
    arrival: u64,
    burst: u64,
}

struct Config {
    process_count: usize,
    runfor: u64,
    algorithm: String,
    quantum: Option<u64>,
    processes: Vec<ProcSpec>,
}

fn parse_input(path: &str) -> Result<Config, String> {
    let content = fs::read_to_string(path).map_err(|e| format!("cannot read input file: {e}"))?;

    let mut process_count = None;
    let mut runfor = None;
    let mut algorithm = None;
    let mut quantum = None;
    let mut processes = Vec::new();

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let tokens: Vec<&str> = line.split_whitespace().collect();
        match tokens[0] {
            "processcount" => process_count = tokens.get(1).and_then(|s| s.parse().ok()),
            "runfor" => runfor = tokens.get(1).and_then(|s| s.parse().ok()),
            "use" => algorithm = tokens.get(1).map(|s| s.to_string()),
            "quantum" => quantum = tokens.get(1).and_then(|s| s.parse().ok()),
            "process" => {
                let mut name = None;
                let mut arrival = None;
                let mut burst = None;
                let mut i = 1;
                while i + 1 < tokens.len() + 1 && i + 1 <= tokens.len() {
                    if i + 1 >= tokens.len() {
                        break;
                    }
                    match tokens[i] {
                        "name" => name = Some(tokens[i + 1].to_string()),
                        "arrival" => arrival = tokens[i + 1].parse().ok(),
                        "burst" => burst = tokens[i + 1].parse().ok(),
                        _ => {}
                    }
                    i += 2;
                }
                match (name, arrival, burst) {
                    (Some(n), Some(a), Some(b)) => processes.push(ProcSpec {
                        name: n,
                        arrival: a,
                        burst: b,
                    }),
                    (None, _, _) => return Err("Error: Missing parameter name".to_string()),
                    (_, None, _) => return Err("Error: Missing parameter arrival".to_string()),
                    (_, _, None) => return Err("Error: Missing parameter burst".to_string()),
                }
            }
            "end" => break,
            _ => {}
        }
    }

    let process_count =
        process_count.ok_or_else(|| "Error: Missing parameter processcount".to_string())?;
    let runfor = runfor.ok_or_else(|| "Error: Missing parameter runfor".to_string())?;
    let algorithm = algorithm.ok_or_else(|| "Error: Missing parameter use".to_string())?;
    if algorithm == "rr" && quantum.is_none() {
        return Err("Error: Missing quantum parameter when use is 'rr'".to_string());
    }

    Ok(Config {
        process_count,
        runfor,
        algorithm,
        quantum,
        processes,
    })
}

struct RuntimeProc {
    spec: ProcSpec,
    pid: Option<Pid>,
    remaining: u64,
    finished: bool,
    first_run_tick: Option<u64>,
    finish_tick: Option<u64>,
}

fn algorithm_label(algo: &str) -> &'static str {
    match algo {
        "fcfs" => "Using First-Come First-Served",
        "sjf" => "Using preemptive Shortest Job First",
        "rr" => "Using Round Robin",
        _ => "Using Unknown",
    }
}

fn real_tick(epoch: &Instant) -> u64 {
    now_ms(epoch) / TICK_MS
}

fn fcfs(
    ci: usize,
    _queue: &mut Vec<usize>,
    _procs: &[RuntimeProc],
    _config: &Config,
    _rr_elapsed: &mut u64,
) -> Option<usize> {
    // Because FCFS is non-preemptive, the current process runs until completion,
    // since it arrived first. Therefore, we return ci.
    return Some(ci);
}

fn sjf(
    ci: usize,
    queue: &mut Vec<usize>,
    procs: &[RuntimeProc],
    _config: &Config,
    _rr_elapsed: &mut u64,
) -> Option<usize> {
    // Iterates through the queue of ready processes
    for &i in queue.iter() {
        // Checks if the remaining time of the current process is greater
        // than the remaining time of the process at index i in the queue
        if(procs[i].remaining < procs[ci].remaining) {
            // Preempts the current process and adds it back to the queue
            preempt(procs[ci].pid.unwrap());
            queue.push(ci);
            // Returns none, so the scheduler can select the next process to run
            return None;
        }
    }
    // If the remaining time of the current process is less than
    // or equal to the remaining time of any process
    // in the queue, we continue running the current process
    return Some(ci);
}

fn rr(
    ci: usize,
    queue: &mut Vec<usize>,
    procs: &[RuntimeProc],
    config: &Config,
    rr_elapsed: &mut u64,
) -> Option<usize> {
    // Checks if the elapsed time for current turn is greater than
    // or equal to the quantum, and if the queue is not empty
    if(*rr_elapsed >= config.quantum.unwrap() && !queue.is_empty()) {
        // Preempts the current process and adds it back to the queue
        preempt(procs[ci].pid.unwrap());
        queue.push(ci);
        // Returns none, so the scheduler can select the next
        // process to run, just like SJF
        return None;
    }
    // If the current process has not yet exceeded its quantum,
    // we continue running the current process
    return Some(ci);
}

fn run_scheduler(config: &Config, out: &mut File) {
    let mut procs: Vec<RuntimeProc> = config
        .processes
        .iter()
        .cloned()
        .map(|spec| RuntimeProc {
            spec,
            pid: None,
            remaining: 0, // set once the process arrives and we know its burst
            finished: false,
            first_run_tick: None,
            finish_tick: None,
        })
        .collect();

    let mut lines: Vec<String> = Vec::new();
    lines.push(format!("{} processes", config.process_count));
    lines.push(algorithm_label(&config.algorithm).to_string());
    if let Some(q) = config.quantum {
        lines.push(format!("Quantum   {}", q));
    }

    let wpath = workload_path();
    let epoch = Instant::now();
    let mut queue: Vec<usize> = Vec::new();
    let mut current: Option<usize> = None;
    let mut rr_elapsed: u64 = 0;

    for tick in 0..config.runfor {
        // Pace to real time: sleep until this tick's wall-clock boundary.
        let target = Duration::from_millis(tick * TICK_MS);
        let elapsed = epoch.elapsed();
        if target > elapsed {
            thread::sleep(target - elapsed);
        }

        // 1. Arrivals at this tick: fork now (stopped before exec; it execs
        //    when first selected), not earlier -- process creation
        //    coincides with logical arrival.
        for i in 0..procs.len() {
            if procs[i].spec.arrival == tick && procs[i].pid.is_none() {
                let pid = spawn_paused(&wpath, procs[i].spec.burst);
                procs[i].pid = Some(pid);
                procs[i].remaining = procs[i].spec.burst;
                queue.push(i);
                lines.push(format!(
                    "Time {:3} : {} arrived",
                    real_tick(&epoch),
                    procs[i].spec.name
                ));
            }
        }

        // 2. Detect completion of the currently running process.
        if let Some(ci) = current {
            if procs[ci].remaining == 0 {
                reap_blocking(procs[ci].pid.unwrap());
                procs[ci].finished = true;
                let ft = real_tick(&epoch);
                procs[ci].finish_tick = Some(ft);
                lines.push(format!("Time {:3} : {} finished", ft, procs[ci].spec.name));
                current = None;
            }
        }

        // 3. Preemption (sjf: shorter job arrived; rr: quantum expired; default (_): implies fcfs).
        if let Some(ci) = current {
            current = match config.algorithm.as_str() {
                "sjf" => sjf(ci, &mut queue, &procs, config, &mut rr_elapsed),
                "rr" => rr(ci, &mut queue, &procs, config, &mut rr_elapsed),
                _ => fcfs(ci, &mut queue, &procs, config, &mut rr_elapsed),
            };
        }

        // 4. Select a new process if none is running.
        if current.is_none() && !queue.is_empty() {
            if config.algorithm == "sjf" {
                queue.sort_by_key(|&i| (procs[i].remaining, procs[i].spec.name.clone()));
            }
            let idx = queue.remove(0);
            resume(procs[idx].pid.unwrap());
            current = Some(idx);
            rr_elapsed = 0;
            if procs[idx].first_run_tick.is_none() {
                procs[idx].first_run_tick = Some(tick);
            }
            lines.push(format!(
                "Time {:3} : {} selected (burst {:3})",
                real_tick(&epoch),
                procs[idx].spec.name,
                procs[idx].remaining
            ));
        }

        // 5. Idle, or account this tick of execution.
        if let Some(ci) = current {
            procs[ci].remaining -= 1;
            rr_elapsed += 1;
        } else {
            lines.push(format!("Time {:3} : Idle", real_tick(&epoch)));
        }
    }

    // Final completion check for a process finishing exactly at runfor.
    if let Some(ci) = current {
        if procs[ci].remaining == 0 {
            reap_blocking(procs[ci].pid.unwrap());
            procs[ci].finished = true;
            procs[ci].finish_tick = Some(config.runfor);
            lines.push(format!(
                "Time {:3} : {} finished",
                config.runfor, procs[ci].spec.name
            ));
        }
    }

    lines.push(format!("Finished at time {:3}", config.runfor));
    lines.push(String::new());

    for p in &procs {
        if p.finished {
            let turnaround = p.finish_tick.unwrap() - p.spec.arrival;
            let wait = turnaround - p.spec.burst;
            if let Some(fr) = p.first_run_tick {
                let response = fr - p.spec.arrival;
                lines.push(format!(
                    "{} wait {:3} turnaround {:3}  response {}",
                    p.spec.name, wait, turnaround, response
                ));
            } else {
                lines.push(format!(
                    "{} wait {:3} turnaround {:3}",
                    p.spec.name, wait, turnaround
                ));
            }
        } else {
            lines.push(format!("{} did not finish", p.spec.name));
        }
    }

    for line in &lines {
        writeln!(out, "{}", line).expect("failed to write output");
    }

    // Cleanup: every child we spawned must be gone from the process table
    // before we exit. Anything that didn't finish within runfor (still
    // running, or stopped in the ready queue) is SIGKILLed AND then reaped
    // with a blocking wait. SIGKILL alone would leave a <defunct> zombie,
    // because only the parent's wait removes a dead child's table entry.
    for p in &procs {
        if !p.finished {
            if let Some(pid) = p.pid {
                if let Err(e) = terminate_and_reap(pid) {
                    eprintln!("cleanup: failed to kill/reap {} (pid {}): {}", p.spec.name, pid, e);
                }
            }
        }
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    if args.len() != 2 {
        eprintln!("Usage: scheduler-gpt <input file>");
        process::exit(1);
    }

    let input_path = &args[1];
    let config = match parse_input(input_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("{e}");
            process::exit(1);
        }
    };

    let out_path = Path::new(input_path).with_extension("out");
    let mut out = File::create(&out_path).expect("cannot create output file");

    run_scheduler(&config, &mut out);
}
