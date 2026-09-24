use std::cmp::Reverse;
use std::collections::BinaryHeap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::check::{Check, Checks};
use crate::config::Config;
use crate::metadata::{parse_duration, Meta};
use crate::runner::{run_check, RunOutcome};
use crate::state::{StateStore, States};
use crate::telegram::TelegramClient;

pub enum FsAction {
    Upsert(PathBuf),
    Remove(PathBuf),
}

pub enum Event {
    Fs(FsAction),
    Done {
        id: String,
        gen: u64,
        outcome: RunOutcome,
    },
}

// (when, version, id) — smaller Instant pops first
type HeapEntry = Reverse<(Instant, u64, String)>;

pub struct Engine {
    pub checks: Checks,
    pub states: States,
    store: StateStore,
    heap: BinaryHeap<HeapEntry>,
    tx: UnboundedSender<Event>,
    tg: Option<TelegramClient>,
    libexec_dir: Option<PathBuf>,
    default_period: Duration,
    default_timeout: Duration,
    recheck_interval: Duration,
    gen_counter: u64,
}

impl Engine {
    pub fn new(cfg: &Config, tx: UnboundedSender<Event>) -> Result<Self> {
        let tg = match &cfg.telegram {
            Some(t) => Some(TelegramClient::new(t.bot_token.clone(), t.chat_id.clone())?),
            None => {
                tracing::warn!("no telegram config: alerts will only be logged");
                None
            }
        };
        Ok(Engine {
            checks: Checks::new(),
            states: States::new(),
            store: StateStore::new(cfg.state_dir()),
            heap: BinaryHeap::new(),
            tx,
            tg,
            libexec_dir: cfg.libexec_dir.clone(),
            default_period: cfg
                .defaults
                .period
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .unwrap_or(Duration::from_secs(300)),
            default_timeout: cfg
                .defaults
                .timeout
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .unwrap_or(Duration::from_secs(60)),
            recheck_interval: cfg
                .defaults
                .recheck
                .as_deref()
                .map(parse_duration)
                .transpose()?
                .unwrap_or(Duration::from_secs(60)),
            gen_counter: 0,
        })
    }

    pub fn scan_dir(&mut self, dir: &Path) {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!("cannot read checks dir {}: {e}", dir.display());
                return;
            }
        };
        for entry in entries.flatten() {
            self.upsert(&entry.path());
        }
        // run everything shortly after startup
        for id in self.checks.keys().cloned().collect::<Vec<_>>() {
            self.schedule_at(&id, Instant::now());
        }
        // load persistent state after checks so next_run_at from disk is not clobbered
        // (states are loaded before scan in main; here only for safety)
    }

    pub fn set_states(&mut self, states: States) {
        self.states = states;
    }

    pub fn handle_fs(&mut self, action: FsAction) {
        match action {
            FsAction::Upsert(path) => self.upsert(&path),
            FsAction::Remove(path) => {
                if let Some(id) = file_id(&path) {
                    if self.checks.remove(&id).is_some() {
                        tracing::info!("check unloaded: {id}");
                    }
                }
            }
        }
    }

    fn upsert(&mut self, path: &Path) {
        let Some(id) = file_id(path) else { return };
        // rate-limit reload storms from editors writing + renaming
        if let Some(existing) = self.checks.get(&id) {
            if existing.last_reload.elapsed() < Duration::from_millis(500) {
                return;
            }
        }
        if !path.is_file() {
            return;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            match std::fs::metadata(path) {
                Ok(md) if md.permissions().mode() & 0o111 != 0 => {}
                _ => {
                    tracing::debug!("skipping non-executable file: {}", path.display());
                    return;
                }
            }
        }
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) => {
                tracing::warn!("cannot read check {}: {e}", path.display());
                return;
            }
        };
        let meta = Meta::parse(&text);
        match self.checks.get_mut(&id) {
            Some(existing) => {
                existing.version += 1;
                existing.meta = meta;
                existing.path = path.to_path_buf();
                existing.last_reload = Instant::now();
                tracing::info!("check reloaded: {id}");
            }
            None => {
                self.checks.insert(
                    id.clone(),
                    Check::new(id.clone(), path.to_path_buf(), meta, 1),
                );
                tracing::info!("check loaded: {id}");
            }
        }
        self.states.entry(id.clone()).or_default();
        // a new/reloaded check runs immediately (stale heap entries are invalidated by version)
        self.schedule_at(&id, Instant::now());
    }

    fn schedule_at(&mut self, id: &str, when: Instant) {
        let Some(check) = self.checks.get(id) else {
            return;
        };
        let version = check.version;
        self.heap.push(Reverse((when, version, id.to_string())));
        self.persist_next_run(id, when);
    }

    fn persist_next_run(&mut self, id: &str, when: Instant) {
        if let Some(state) = self.states.get_mut(id) {
            state.next_run_at = Some(when_to_epoch(when));
            self.store.save(id, state);
        }
    }

    /// pop and launch all due checks
    pub fn run_due(&mut self) {
        let now = Instant::now();
        loop {
            let due = match self.heap.peek() {
                Some(Reverse((when, version, id))) if *when <= now => (*version, id.clone()),
                _ => break,
            };
            self.heap.pop();
            let (version, id) = due;
            let Some(check) = self.checks.get(&id) else {
                continue;
            };
            if check.version != version {
                continue; // stale entry from before a reload
            }
            if check.running_gen.is_some() {
                continue; // already running; its Done event reschedules
            }
            self.spawn_run(&id);
        }
    }

    fn spawn_run(&mut self, id: &str) {
        let Some(check) = self.checks.get_mut(id) else {
            return;
        };
        self.gen_counter += 1;
        let gen = self.gen_counter;
        check.running_gen = Some(gen);

        let timeout = check
            .meta
            .timeout
            .as_deref()
            .map(parse_duration)
            .transpose()
            .ok()
            .flatten()
            .unwrap_or(self.default_timeout);
        let path = check.path.clone();
        let vars = check.meta.vars.clone();
        let libexec = self.libexec_dir.clone();
        let tx = self.tx.clone();
        let id = id.to_string();

        tracing::debug!("running check {id}");
        tokio::spawn(async move {
            let outcome = run_check(&path, timeout, &vars, libexec.as_deref()).await;
            let _ = tx.send(Event::Done { id, gen, outcome });
        });
    }

    pub fn handle_done(&mut self, id: &str, gen: u64, outcome: RunOutcome) {
        let Some(check) = self.checks.get_mut(id) else {
            return;
        };
        if check.running_gen != Some(gen) {
            return; // stale result from a previous run
        }
        check.running_gen = None;

        let ok = outcome.ok();
        let meta = check.meta.clone();
        let state = self.states.entry(id.to_string()).or_default();
        let now = Instant::now();

        let next: Duration;
        if ok {
            state.pending_flake = false;
            if state.alert_active {
                state.alert_active = false;
                state.repeat_index = 0;
                tracing::info!("check recovered: {id}");
                if meta.report_restored {
                    self.send_notify(id, format!("🟢 {id}: restored"));
                }
            }
            next = duration_of(&meta.period, self.default_period);
        } else {
            let flake = meta
                .flake
                .as_deref()
                .map(parse_duration)
                .transpose()
                .ok()
                .flatten();
            if let Some(flake) = flake.filter(|_| !state.pending_flake && !state.alert_active) {
                // first failure: silent retry after the flake window
                state.pending_flake = true;
                tracing::info!("check {id} failed, flake-retry in {:?}", flake);
                next = flake;
            } else {
                state.pending_flake = false;
                if !state.alert_active {
                    state.alert_active = true;
                    state.repeat_index = 0;
                    state.alert_count += 1;
                    state.last_alert_at = Some(epoch_now());
                    tracing::warn!(
                        "check failed: {id} (code={:?} timed_out={})",
                        outcome.code,
                        outcome.timed_out
                    );
                    self.send_notify(id, format_alert(id, &meta, &outcome));
                    next = self.recheck_interval;
                } else {
                    // still failing: re-send per the escalation schedule
                    let schedule: Vec<Duration> = meta
                        .repeat
                        .iter()
                        .map(|s| parse_duration(s))
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap_or_default();
                    if let Some(need) = schedule
                        .get(state.repeat_index.min(schedule.len().saturating_sub(1)))
                        .copied()
                    {
                        let since = state
                            .last_alert_at
                            .map(|t| epoch_now() - t)
                            .unwrap_or(i64::MAX);
                        if since >= need.as_secs() as i64 {
                            state.repeat_index += 1;
                            state.alert_count += 1;
                            state.last_alert_at = Some(epoch_now());
                            tracing::warn!("repeat alert: {id}");
                            self.send_notify(id, format_alert(id, &meta, &outcome));
                        }
                    }
                    next = self.recheck_interval;
                }
            }
        }

        self.schedule_at(id, now + next);
    }

    fn send_notify(&mut self, id: &str, text: String) {
        match &self.tg {
            Some(tg) => {
                let tg = tg.clone();
                tokio::spawn(async move {
                    if let Err(e) = tg.send(&text).await {
                        tracing::error!("telegram send failed: {e}");
                    }
                });
            }
            None => tracing::info!("[notify] {id}: {text}"),
        }
    }
}

fn duration_of(opt: &Option<String>, default: Duration) -> Duration {
    opt.as_deref()
        .map(parse_duration)
        .transpose()
        .ok()
        .flatten()
        .unwrap_or(default)
}

fn epoch_now() -> i64 {
    when_to_epoch(Instant::now())
}

fn when_to_epoch(_when: Instant) -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// id = file name inside the watched dir (hidden files, dirs and editor temps are skipped)
fn file_id(path: &Path) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    if name.starts_with('.') || name.ends_with('~') || name.ends_with(".tmp") {
        return None;
    }
    Some(name.to_string())
}

fn tail(s: &str, max_chars: usize) -> String {
    let s = s.trim_end();
    let count = s.chars().count();
    if count <= max_chars {
        s.to_string()
    } else {
        let cut: String = s.chars().skip(count - max_chars).collect();
        format!("…{cut}")
    }
}

fn format_alert(id: &str, meta: &Meta, outcome: &RunOutcome) -> String {
    let exitcode = match (outcome.timed_out, outcome.code) {
        (true, _) => "TIMEOUT".to_string(),
        (false, Some(c)) => c.to_string(),
        (false, None) => "signal".to_string(),
    };
    let stdout = tail(&outcome.stdout, 1000);
    let stderr = tail(&outcome.stderr, 1500);
    match &meta.message {
        Some(tmpl) => tmpl
            .replace("$name", id)
            .replace("$exitcode", &exitcode)
            .replace("$stdout", &stdout)
            .replace("$stderr", &stderr),
        None => {
            let mut msg = format!("🔴 {id}: check failed (exit={exitcode})");
            if !stderr.is_empty() {
                msg.push_str(&format!("\n{stderr}"));
            } else if !stdout.is_empty() {
                msg.push_str(&format!("\n{stdout}"));
            }
            msg
        }
    }
}
