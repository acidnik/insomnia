use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use tokio::sync::mpsc::UnboundedSender;

use crate::check::{Check, Checks};
use crate::config::Config;
use crate::metadata::{parse_duration, Meta};
use crate::quiet::{self, QuietWindow};
use crate::runner::{run_check, RunOutcome};
use crate::state::{StateStore, States};
use crate::telegram::TelegramClient;

pub enum FsAction {
    Upsert(PathBuf),
    Remove(PathBuf),
}

pub enum Event {
    Fs(FsAction),
    /// debounced re-read of an edited check; `token` identifies the debounce
    /// window it was scheduled in, so timers superseded by a fresher fs event
    /// can be dropped
    FsRecheck { path: PathBuf, token: u64 },
    /// ask main to (un)watch a check's symlink target file
    Watch { path: PathBuf, add: bool },
    Done {
        id: String,
        gen: u64,
        outcome: RunOutcome,
    },
}

/// a save in an editor is a burst of inotify events (temp file, write, chmod,
/// rename, ...) — the check is re-read only after the burst has been quiet
/// for this long
const RELOAD_DEBOUNCE: Duration = Duration::from_secs(1);

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
    checks_dir: PathBuf,
    /// check id -> canonical path of its symlink target (only for symlinks)
    target_of: HashMap<String, PathBuf>,
    /// canonical target path -> check id
    target_id: HashMap<PathBuf, String>,
    /// canonical target parent dir -> number of checks living in it
    watched_dirs: HashMap<PathBuf, usize>,
    /// check id -> token of its newest pending debounce timer
    pending_reloads: HashMap<String, u64>,
    default_period: Duration,
    default_timeout: Duration,
    /// daily window with no notifications at all: checks keep running and
    /// updating state, only the delivery waits (see quiet.rs)
    quiet: Option<QuietWindow>,
    /// last seen state of that window, for the start/end log lines
    in_quiet: bool,
    gen_counter: u64,
    reload_token: u64,
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
        // a malformed window is fatal, not a warning: a typo here would
        // silently wake the user up all night
        let quiet = QuietWindow::parse(cfg.quiet_time.as_ref())?;
        if let Some(q) = &quiet {
            tracing::info!("quiet hours: {} local time", q.label());
        }
        let in_quiet = quiet
            .map(|q| q.active_at(quiet::local_minute()))
            .unwrap_or(false);
        Ok(Engine {
            checks: Checks::new(),
            states: States::new(),
            store: StateStore::new(cfg.state_dir()),
            heap: BinaryHeap::new(),
            tx,
            tg,
            libexec_dir: cfg.libexec_dir.clone(),
            checks_dir: cfg.checks_dir.clone(),
            target_of: HashMap::new(),
            target_id: HashMap::new(),
            watched_dirs: HashMap::new(),
            pending_reloads: HashMap::new(),
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
            quiet,
            in_quiet,
            gen_counter: 0,
            reload_token: 0,
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
        // startup loads directly: the debounce is for fs events during the run
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(id) = file_id(&path) {
                self.reload(&path, id);
            }
        }
        // scheduling happens inside reload(): every check (new or existing)
        // is scheduled from its last_run_at + period, so startup never
        // mass-runs checks and edited periods apply from the last run moment
    }

    pub fn set_states(&mut self, states: States) {
        self.states = states;
    }

    pub fn handle_fs(&mut self, action: FsAction) {
        match action {
            // trace, not debug: one editor save is a dozen events, and the
            // unit runs with RUST_LOG=debug
            FsAction::Upsert(path) => {
                tracing::trace!("fs event: upsert {}", path.display());
                self.upsert(&path)
            }
            FsAction::Remove(path) => {
                tracing::trace!("fs event: remove {}", path.display());
                self.remove(&path)
            }
        }
    }

    /// fs event inside the watched dir
    fn upsert(&mut self, path: &Path) {
        // events inside a watched target dir: debounce every check living there
        // (the event may be for the target itself or for an editor temp file —
        // re-reading all of the dir's checks is cheap, and the debounce turns
        // the burst into a single reload)
        if let Some(dir) = path.parent() {
            if self.watched_dirs.contains_key(dir) {
                let ids: Vec<String> = self
                    .target_of
                    .iter()
                    .filter(|(_, t)| t.parent() == Some(dir))
                    .map(|(id, _)| id.clone())
                    .collect();
                for id in ids {
                    if let Some(p) = self.checks.get(&id).map(|c| c.path.clone()) {
                        self.request_reload(&p);
                    }
                }
                return;
            }
        }
        // editor temp files are filtered out by file_id: the rename onto the
        // real name is the event that matters
        self.request_reload(path);
    }

    /// debounce window elapsed for this check — the fs event that started it
    /// has not been followed by a fresher one
    pub fn handle_recheck(&mut self, path: &Path, token: u64) {
        let Some(id) = file_id(path) else { return };
        if self.pending_reloads.get(&id) != Some(&token) {
            return; // a fresher fs event restarted the window
        }
        self.pending_reloads.remove(&id);
        if !path.is_file() {
            return; // removed in the meantime — do not resurrect
        }
        self.reload(path, id);
    }

    /// (re)start the debounce window; only the timer started by the last fs
    /// event of a burst passes the token check in handle_recheck
    fn request_reload(&mut self, path: &Path) {
        let Some(id) = file_id(path) else { return };
        self.reload_token += 1;
        let token = self.reload_token;
        self.pending_reloads.insert(id, token);
        let tx = self.tx.clone();
        let path = path.to_path_buf();
        tokio::spawn(async move {
            tokio::time::sleep(RELOAD_DEBOUNCE).await;
            let _ = tx.send(Event::FsRecheck { path, token });
        });
    }

    fn remove(&mut self, path: &Path) {
        // only the checks_dir entry itself counts; deleting a symlink's target
        // file elsewhere must not unload the check
        if !self.in_checks_dir(path) {
            return;
        }
        let Some(id) = file_id(path) else { return };
        if self.checks.remove(&id).is_some() {
            tracing::info!("check unloaded: {id}");
            if let Some(target) = self.target_of.remove(&id) {
                self.target_id.remove(&target);
                self.unwatch_dir_for(&target);
            }
        }
    }

    /// events are reported with the path spelling notify uses, which is not
    /// necessarily the config spelling (a relative checks_dir comes back
    /// absolute from a dir watch), so compare canonical paths
    fn in_checks_dir(&self, path: &Path) -> bool {
        let (Ok(dir), Some(parent)) = (self.checks_dir.canonicalize(), path.parent()) else {
            return false;
        };
        parent.canonicalize().map(|p| p == dir).unwrap_or(false)
    }

    fn reload(&mut self, path: &Path, id: String) {
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
        self.watch_target(&id);
        // keep the last-run schedule: after an edit the check runs only if
        // the (possibly shortened) period has already elapsed since the last
        // run; a never-run check starts immediately
        let period = duration_of(
            &self.checks.get(&id).expect("just inserted").meta.period,
            self.default_period,
        );
        let due_in = match self.states.get(&id).and_then(|s| s.last_run_at) {
            None => Duration::ZERO,
            Some(last) => period
                .saturating_sub(Duration::from_secs((epoch_now() - last).max(0) as u64)),
        };
        self.schedule_at(&id, Instant::now() + due_in);
    }

    /// keep an inotify watch on the DIR containing a symlinked check's real
    /// file (not the file itself: editors replace the file via rename, which
    /// silently kills a file watch, while a dir watch survives)
    fn watch_target(&mut self, id: &str) {
        let Some(check) = self.checks.get(id) else { return };
        // only symlinked checks get a dir watch on their target: a regular file
        // living in the checks dir is already covered by the dir watch.
        // Detected by the symlink bit, not by comparing paths — with a relative
        // checks_dir every file would look like it points outside its own dir
        let is_symlink = std::fs::symlink_metadata(&check.path)
            .map(|md| md.is_symlink())
            .unwrap_or(false);
        let new_target = if is_symlink {
            std::fs::canonicalize(&check.path).ok()
        } else {
            None
        };
        if self.target_of.get(id).map(|p| p.as_path()) == new_target.as_deref() {
            return;
        }
        if let Some(old) = self.target_of.remove(id) {
            self.target_id.remove(&old);
            self.unwatch_dir_for(&old);
        }
        if let Some(new) = new_target {
            let dir = new.parent().unwrap_or(Path::new("/")).to_path_buf();
            self.target_id.insert(new.clone(), id.to_string());
            self.target_of.insert(id.to_string(), new);
            let count = self.watched_dirs.entry(dir.clone()).or_insert(0);
            if *count == 0 {
                let _ = self.tx.send(Event::Watch { path: dir, add: true });
            }
            *count += 1;
        }
    }

    /// drop one reference to a target's dir watch; unwatch when the last
    /// check living in that dir is gone
    fn unwatch_dir_for(&mut self, target: &Path) {
        let Some(dir) = target.parent().map(|p| p.to_path_buf()) else { return };
        let Some(count) = self.watched_dirs.get_mut(&dir) else { return };
        *count -= 1;
        if *count == 0 {
            self.watched_dirs.remove(&dir);
            let _ = self.tx.send(Event::Watch { path: dir, add: false });
        }
    }

    fn schedule_at(&mut self, id: &str, when: Instant) {
        let Some(check) = self.checks.get(id) else {
            return;
        };
        let version = check.version;
        self.heap.push(Reverse((when, version, id.to_string())));
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
        let id_owned = id.to_string();

        // schedule durability: save the run start before running, so a
        // restart in the middle of a run does not consider it overdue (the
        // state after the run is saved in handle_done)
        let state = self.states.entry(id.to_string()).or_default();
        state.last_run_at = Some(epoch_now());
        self.store.save(id, state);

        tracing::info!("running check {id}");
        tokio::spawn(async move {
            let outcome = run_check(&path, timeout, &vars, libexec.as_deref()).await;
            let _ = tx.send(Event::Done { id: id_owned, gen, outcome });
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
        tracing::debug!(
            "check {id} finished in {}: code={:?} timed_out={} \
             stdout={} stderr={}",
            fmt_secs(outcome.duration),
            outcome.code,
            outcome.timed_out,
            tail(&outcome.stdout, 300),
            tail(&outcome.stderr, 300),
        );
        let meta = check.meta.clone();
        // decided before the state borrow starts
        let quiet = self.quiet_now();
        let state = self.states.entry(id.to_string()).or_default();
        let now = Instant::now();

        let period_dur = duration_of(&meta.period, self.default_period);
        let next: Duration;
        if ok {
            state.pending_flake = false;
            // the incident ends here even if it never got past the flake
            // retry; take() also clears failing_since, and the borrow must
            // end before send_notify (&mut self)
            let downtime = state
                .failing_since
                .take()
                .map(|s| epoch_now() - s)
                .map(|secs| Duration::from_secs(secs.max(0) as u64));
            if state.alert_active {
                state.alert_active = false;
                state.repeat_index = 0;
                tracing::info!("check recovered: {id}");
                // a first alert the quiet hours never delivered: the incident
                // is over before the user heard of it, so say nothing at all.
                // A held-back *repeat* means the alert itself did go out
                // earlier — that incident gets its restore in the morning.
                let unannounced = state.deferred_alert.is_some() && !state.deferred_alert_repeat;
                state.deferred_alert = None;
                state.deferred_alert_repeat = false;
                if meta.report_restored && !unannounced {
                    let name = meta.name.as_deref().unwrap_or(id);
                    let after = match downtime {
                        Some(d) => format!(" after {}", fmt_human(d)),
                        None => String::new(),
                    };
                    let text = format!("🟢 {name}: restored{after}");
                    if quiet {
                        // the alert went out before the quiet hours: closing
                        // the incident in the morning beats leaving it looking
                        // like it is still down
                        state.deferred_restored = Some(text);
                    } else {
                        self.send_notify(id, text);
                    }
                }
            }
            next = period_dur;
        } else {
            // the incident starts at the first failed run, not at the alert:
            // repeat and restored messages count the downtime from here
            // (flake retries included)
            let down_since = *state.failing_since.get_or_insert(epoch_now());
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
                    tracing::warn!(
                        "check failed: {id} (code={:?} timed_out={})",
                        outcome.code,
                        outcome.timed_out
                    );
                    let body = format_alert(id, &meta, &outcome);
                    if quiet {
                        // held back until the window ends; counted and
                        // escalated when it is actually delivered
                        tracing::debug!("quiet hours: holding back alert for {id}");
                        state.deferred_alert = Some(body);
                        state.deferred_alert_repeat = false;
                    } else {
                        state.alert_count += 1;
                        state.last_alert_at = Some(epoch_now());
                        self.send_notify(id, body);
                    }
                    // while alerting, re-check every `recheck` (default = the check's period)
                    next = duration_of(&meta.recheck, period_dur);
                } else {
                    // still failing: re-send per the escalation schedule
                    let schedule: Vec<Duration> = meta
                        .repeat
                        .iter()
                        .map(|s| parse_duration(s))
                        .collect::<Result<Vec<_>, _>>()
                        .unwrap_or_default();
                    let need = schedule
                        .get(state.repeat_index.min(schedule.len().saturating_sub(1)))
                        .copied();
                    let since = state
                        .last_alert_at
                        .map(|t| epoch_now() - t)
                        .unwrap_or(i64::MAX);
                    let due = need.is_some_and(|n| since >= n.as_secs() as i64);
                    if due {
                        if quiet {
                            // a repeat that comes due overnight is not dropped,
                            // it waits here and goes out with the summary. The
                            // held-back first alert keeps its body (that is when
                            // the incident started), a held-back repeat is
                            // refreshed with the newest reason.
                            if state.deferred_alert.is_none() {
                                state.deferred_alert = Some(format_alert(id, &meta, &outcome));
                                state.deferred_alert_repeat = true;
                            } else if state.deferred_alert_repeat {
                                state.deferred_alert = Some(format_alert(id, &meta, &outcome));
                            }
                        } else {
                            state.repeat_index += 1;
                            state.alert_count += 1;
                            state.last_alert_at = Some(epoch_now());
                            tracing::warn!("repeat alert: {id}");
                            let down =
                                Duration::from_secs((epoch_now() - down_since).max(0) as u64);
                            self.send_notify(id, format_repeat_alert(id, &meta, &outcome, down));
                        }
                    }
                    next = duration_of(&meta.recheck, period_dur);
                }
            }
        }

        self.schedule_at(id, now + next);
        // persist the post-run state, not just the run-start snapshot: the
        // run-start save is one outcome stale, so a restart would load an
        // alert that has already been restored (and re-send "restored" with
        // an ever-growing downtime) or lose a brand-new one
        if let Some(state) = self.states.get(id) {
            self.store.save(id, state);
        }
    }

    /// is it quiet right now? (checks keep running either way — only the
    /// delivery waits)
    fn quiet_now(&self) -> bool {
        self.quiet
            .map(|q| q.active_at(quiet::local_minute()))
            .unwrap_or(false)
    }

    /// once a second: notice the window edges and, while it is closed, deliver
    /// whatever the quiet hours held back. Running the flush outside the window
    /// also covers a restart that outlived the window end with messages still
    /// parked on disk.
    pub fn check_quiet_window(&mut self) {
        let Some(q) = self.quiet else { return };
        let now_quiet = q.active_at(quiet::local_minute());
        if now_quiet != self.in_quiet {
            self.in_quiet = now_quiet;
            if now_quiet {
                tracing::info!("quiet hours started ({} local time)", q.label());
            } else {
                tracing::info!("quiet hours over ({} local time)", q.label());
            }
        }
        if !now_quiet {
            self.flush_deferred();
        }
    }

    /// deliver everything the quiet hours held back as one summary. Each alert
    /// repeats the escalation bookkeeping its own send would have done, so the
    /// schedule counts from the delivery and not from the moment it was due.
    fn flush_deferred(&mut self) {
        let mut alerts: Vec<(String, Duration)> = Vec::new();
        let mut restored = Vec::new();
        let mut touched = Vec::new();
        for (id, state) in self.states.iter_mut() {
            if let Some(text) = state.deferred_restored.take() {
                restored.push(text);
            }
            let Some(body) = state.deferred_alert.take() else {
                continue;
            };
            let down = Duration::from_secs(
                state
                    .failing_since
                    .map(|s| (epoch_now() - s).max(0) as u64)
                    .unwrap_or(0),
            );
            alerts.push((body, down));
            state.alert_count += 1;
            state.last_alert_at = Some(epoch_now());
            if state.deferred_alert_repeat {
                state.repeat_index += 1;
            }
            state.deferred_alert_repeat = false;
            touched.push(id.clone());
        }
        if alerts.is_empty() && restored.is_empty() {
            return;
        }
        let msgs = summary_messages(
            &self.quiet.map(|q| q.label()).unwrap_or_default(),
            &alerts,
            &restored,
        );
        self.send_summary(msgs);
        // the delivery is what the escalation schedule counts from, so persist
        // it. Deliberately after the send: a crash in between costs a repeated
        // summary, the opposite order would lose an alert.
        for id in touched {
            if let Some(state) = self.states.get(&id) {
                self.store.save(&id, state);
            }
        }
    }

    /// one summary, split into several messages only when one would not fit
    fn send_summary(&mut self, msgs: Vec<String>) {
        let Some(tg) = self.tg.clone() else {
            for msg in &msgs {
                tracing::info!("[notify] {msg}");
            }
            return;
        };
        tracing::info!("quiet hours over, sending {} summary message(s)", msgs.len());
        tokio::spawn(async move {
            for msg in msgs {
                if let Err(e) = tg.send(&msg).await {
                    tracing::error!("telegram send failed: {e}");
                }
            }
        });
    }

    fn send_notify(&mut self, id: &str, text: String) {
        match &self.tg {
            Some(tg) => {
                tracing::debug!("alert -> telegram: {text}");
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

/// Instant has no absolute reference point — anchor it to the wall clock on
/// first use, then convert any Instant into unix epoch seconds relative to
/// that anchor
fn when_to_epoch(when: Instant) -> i64 {
    static ANCHOR: std::sync::OnceLock<(Instant, i64)> = std::sync::OnceLock::new();
    let (anchor_instant, anchor_epoch) = ANCHOR.get_or_init(|| {
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        (Instant::now(), secs)
    });
    anchor_epoch + when.saturating_duration_since(*anchor_instant).as_secs() as i64
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

/// uniform duration formatting in logs and alerts: `x.xxs`
fn fmt_secs(d: Duration) -> String {
    format!("{:.2}s", d.as_secs_f64())
}

/// human-readable duration in the config style: `5m 30s`, `1h 2m`, `2d 3h`
fn fmt_human(d: Duration) -> String {
    let total = d.as_secs();
    let days = total / 86400;
    let hours = (total % 86400) / 3600;
    let mins = (total % 3600) / 60;
    let secs = total % 60;
    let mut parts = Vec::new();
    if days > 0 {
        parts.push(format!("{days}d"));
    }
    if hours > 0 {
        parts.push(format!("{hours}h"));
    }
    if mins > 0 {
        parts.push(format!("{mins}m"));
    }
    if secs > 0 || parts.is_empty() {
        parts.push(format!("{secs}s"));
    }
    parts.join(" ")
}

fn format_alert(id: &str, meta: &Meta, outcome: &RunOutcome) -> String {
    // display name replaces the file id where the check provides one
    let id = meta.name.as_deref().unwrap_or(id);
    let dur = fmt_secs(outcome.duration);
    let exitcode = match (outcome.timed_out, outcome.code) {
        (true, _) => format!("TIMEOUT after {dur}"),
        (false, Some(c)) => c.to_string(),
        (false, None) => "signal".to_string(),
    };
    let stdout = tail(&outcome.stdout, 1000);
    let stderr = tail(&outcome.stderr, 1500);
    let mut msg = match &meta.message {
        Some(tmpl) => tmpl
            .replace("$name", id)
            .replace("$exitcode", &exitcode)
            .replace("$stdout", &stdout)
            .replace("$stderr", &stderr),
        None => {
            let mut msg = format!("🔴 {id}: check failed (exit={exitcode})");
            if !stderr.is_empty() {
                msg.push_str(&format!("\n{stderr}"));
            }
            if !stdout.is_empty() {
                msg.push_str(&format!("\n{stdout}"));
            }
            msg
        }
    };
    // a timed-out check usually prints nothing, so custom templates like
    // "... ($stdout)" would end up empty and cryptic — say what happened
    if outcome.timed_out && stdout.is_empty() && stderr.is_empty() {
        msg.push_str(&format!("\n⏱ no output: check hung and was killed after {dur}"));
    }
    msg
}

/// repeat alerts say how long the service has been down: `(down for 2h 30m)`;
/// appended on its own line so custom `# message:` templates stay untouched
fn format_repeat_alert(id: &str, meta: &Meta, outcome: &RunOutcome, down: Duration) -> String {
    summary_entry(&format_alert(id, meta, outcome), down)
}

/// the downtime suffix, shared by repeat alerts and quiet-hours summary
/// entries: the summary groups checks, so each one brings its own line — and
/// with it its own `(down for …)`
fn summary_entry(body: &str, down: Duration) -> String {
    format!("{body}\n(down for {})", fmt_human(down))
}

/// the quiet-hours summary: one block per check, each with its own fresh
/// downtime, alerts first (they need action) and the restores that close older
/// incidents after them
fn summary_messages(label: &str, alerts: &[(String, Duration)], restored: &[String]) -> Vec<String> {
    let mut counts = Vec::new();
    if !alerts.is_empty() {
        counts.push(format!(
            "{} alert{}",
            alerts.len(),
            if alerts.len() == 1 { "" } else { "s" }
        ));
    }
    if !restored.is_empty() {
        counts.push(format!("{} restored", restored.len()));
    }
    let header = format!("🌙 quiet hours {label} over — {}", counts.join(", "));
    let blocks: Vec<String> = alerts
        .iter()
        .map(|(body, down)| summary_entry(body, *down))
        .chain(restored.iter().cloned())
        .collect();
    pack_messages(&header, &blocks)
}

/// Telegram rejects messages over 4096 characters and the client truncates at
/// 3900; a summary that does not fit is split at check boundaries with the
/// header repeated, so nothing is dropped silently
const SUMMARY_CHARS: usize = 3700;

fn pack_messages(header: &str, blocks: &[String]) -> Vec<String> {
    let mut msgs = Vec::new();
    let mut cur = header.to_string();
    for block in blocks {
        let fits = cur.chars().count() + block.chars().count() + 2 <= SUMMARY_CHARS;
        if !fits && cur.chars().count() > header.chars().count() {
            msgs.push(std::mem::take(&mut cur));
            cur = header.to_string();
        }
        // the header opens every message, so every block — including the first
        // one — needs the blank line before it
        cur.push_str("\n\n");
        cur.push_str(block);
    }
    msgs.push(cur);
    msgs
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failed() -> RunOutcome {
        RunOutcome {
            code: Some(1),
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            duration: Duration::from_secs(1),
        }
    }

    fn ok() -> RunOutcome {
        RunOutcome {
            code: Some(0),
            timed_out: false,
            stdout: String::new(),
            stderr: String::new(),
            duration: Duration::from_secs(1),
        }
    }

    #[test]
    fn repeat_alert_appends_downtime() {
        let meta = Meta::parse("# name: web\n");
        assert_eq!(
            format_repeat_alert("web.check.sh", &meta, &failed(), Duration::from_secs(9000)),
            "🔴 web: check failed (exit=1)\n(down for 2h 30m)"
        );
    }

    /// the morning summary groups several checks, so every entry carries its
    /// own downtime line — the same suffix a repeat alert has
    #[test]
    fn every_summary_entry_carries_its_own_downtime() {
        assert_eq!(
            summary_entry("🔴 web: check failed (exit=1)", Duration::from_secs(2700)),
            "🔴 web: check failed (exit=1)\n(down for 45m)"
        );
        assert_eq!(
            summary_entry("🔴 db: check failed (exit=1)", Duration::from_secs(300)),
            "🔴 db: check failed (exit=1)\n(down for 5m)"
        );
    }

    #[test]
    fn long_summaries_split_at_check_boundaries() {
        let header = "🌙 quiet hours 23:00→07:00 over — 2 alerts";
        let blocks = vec!["a".repeat(2000), "b".repeat(2000)];
        let msgs = pack_messages(header, &blocks);
        assert_eq!(msgs.len(), 2);
        for msg in &msgs {
            assert!(
                msg.starts_with(&format!("{header}\n\n")),
                "every message opens with the header and a blank line"
            );
            assert!(msg.chars().count() <= SUMMARY_CHARS);
        }
        assert!(msgs[0].ends_with('a'));
        assert!(msgs[1].ends_with('b'));
    }

    /// the summary groups the held-back checks: one block each, each with its
    /// own downtime, alerts before restores, header counting both
    #[test]
    fn summary_groups_a_line_per_check() {
        let alerts = vec![
            (
                "🔴 web: check failed (exit=1)".to_string(),
                Duration::from_secs(2700),
            ),
            (
                "🔴 db: check failed (exit=1)".to_string(),
                Duration::from_secs(300),
            ),
        ];
        let restored = vec!["🟢 api: restored after 5h".to_string()];
        let msgs = summary_messages("23:00→07:00", &alerts, &restored);
        assert_eq!(msgs.len(), 1, "short entries still fit one message");
        assert_eq!(
            msgs[0],
            "🌙 quiet hours 23:00→07:00 over — 2 alerts, 1 restored\n\n\
             🔴 web: check failed (exit=1)\n(down for 45m)\n\n\
             🔴 db: check failed (exit=1)\n(down for 5m)\n\n\
             🟢 api: restored after 5h"
        );
    }

    /// an editor save is a burst of fs events: the check must be re-read once,
    /// after the burst has been quiet for the debounce window
    #[tokio::test]
    async fn fs_burst_collapses_into_one_reload() {
        use tokio::sync::mpsc;

        let root = std::env::temp_dir().join(format!("insomnia-debounce-{}", std::process::id()));
        let checks_dir = root.join("checks");
        std::fs::create_dir_all(&checks_dir).unwrap();
        let path = checks_dir.join("burst.check.sh");
        std::fs::write(&path, "#!/usr/bin/env bash\n# period: 1h\nexit 0\n").unwrap();
        // checks must be executable, otherwise reload() skips them
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cfg = Config {
            checks_dir: checks_dir.clone(),
            state_dir: Some(root.join("state")),
            libexec_dir: None,
            telegram: None,
            quiet_time: None,
            defaults: crate::config::Defaults::default(),
        };
        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut engine = Engine::new(&cfg, tx).unwrap();
        engine.scan_dir(&checks_dir);
        assert_eq!(engine.checks.get("burst.check.sh").unwrap().version, 1);

        // five events, all inside one debounce window
        for _ in 0..5 {
            engine.upsert(&path);
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        tokio::time::sleep(RELOAD_DEBOUNCE + Duration::from_millis(50)).await;

        let mut timers = Vec::new();
        while let Ok(ev) = rx.try_recv() {
            if let Event::FsRecheck { path, token } = ev {
                timers.push((path, token));
            }
        }
        assert_eq!(timers.len(), 5, "every fs event starts its own timer");
        for (path, token) in timers {
            engine.handle_recheck(&path, token);
        }

        assert_eq!(
            engine.checks.get("burst.check.sh").unwrap().version,
            2,
            "the burst must produce exactly one reload"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// the run-start snapshot lags one outcome behind: persisting only that
    /// snapshot made every daemon restart resurrect an alert that had already
    /// been reported as restored — the repeated "restored after 3h/4h" spam
    /// with a downtime counted from the original incident
    #[tokio::test]
    async fn run_outcome_is_persisted_not_only_the_run_start() {
        use tokio::sync::mpsc;

        let root = std::env::temp_dir().join(format!("insomnia-state-{}", std::process::id()));
        let checks_dir = root.join("checks");
        let state_dir = root.join("state");
        std::fs::create_dir_all(&checks_dir).unwrap();
        let id = "flap.check.sh";
        let path = checks_dir.join(id);
        std::fs::write(&path, "#!/usr/bin/env bash\n# period: 1h\nexit 0\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cfg = Config {
            checks_dir: checks_dir.clone(),
            state_dir: Some(state_dir.clone()),
            libexec_dir: None,
            telegram: None,
            quiet_time: None,
            defaults: crate::config::Defaults::default(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut engine = Engine::new(&cfg, tx).unwrap();
        engine.scan_dir(&checks_dir);

        engine.spawn_run(id);
        let gen = engine.checks.get(id).unwrap().running_gen.unwrap();
        engine.handle_done(id, gen, failed());
        assert!(engine.states.get(id).unwrap().alert_active);

        // the failure itself must be on disk, so a restart does not re-alert
        let persisted = StateStore::new(state_dir.clone()).load_all().unwrap();
        assert!(persisted.get(id).unwrap().alert_active);

        // the next run recovers — the alert is gone in memory by now
        engine.spawn_run(id);
        let gen = engine.checks.get(id).unwrap().running_gen.unwrap();
        engine.handle_done(id, gen, ok());
        assert!(!engine.states.get(id).unwrap().alert_active);

        // what a restart would load
        let persisted = StateStore::new(state_dir).load_all().unwrap();
        let state = persisted.get(id).expect("state file");
        assert!(
            !state.alert_active,
            "a restart must not resurrect an alert that was already restored"
        );
        assert!(state.failing_since.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn fmt_human_matches_config_style() {
        assert_eq!(fmt_human(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_human(Duration::from_secs(90)), "1m 30s");
        assert_eq!(fmt_human(Duration::from_secs(300)), "5m");
        assert_eq!(fmt_human(Duration::from_secs(3725)), "1h 2m 5s");
        assert_eq!(fmt_human(Duration::from_secs(90000)), "1d 1h");
        assert_eq!(fmt_human(Duration::from_millis(240)), "0s");
    }

    /// `HH:MM` for a minute of the day
    fn clock(minute: u32) -> String {
        format!("{:02}:{:02}", minute / 60 % 24, minute % 60)
    }

    /// a quiet window given as offsets in minutes from the local time now
    fn quiet_window_config(from_offset: i64, to_offset: i64) -> crate::config::QuietTimeConfig {
        let now = crate::quiet::local_minute() as i64;
        let at = |offset: i64| clock((now + offset).rem_euclid(1440) as u32);
        crate::config::QuietTimeConfig {
            from: at(from_offset),
            to: at(to_offset),
        }
    }

    /// open now: opened a minute ago, closes in an hour (crossing midnight is
    /// fine, the direction is inferred from from/to)
    fn quiet_now_config() -> crate::config::QuietTimeConfig {
        quiet_window_config(-1, 60)
    }

    /// closed now: opens in a minute
    fn quiet_later_config() -> crate::config::QuietTimeConfig {
        quiet_window_config(1, 2)
    }

    /// engine with one executable check; `quiet` adds a window open right now
    fn engine_with_check(name: &str, quiet: bool) -> (Engine, String, PathBuf) {
        use tokio::sync::mpsc;

        let root = std::env::temp_dir().join(format!("insomnia-{name}-{}", std::process::id()));
        let checks_dir = root.join("checks");
        std::fs::create_dir_all(&checks_dir).unwrap();
        let id = "quiet.check.sh".to_string();
        let path = checks_dir.join(&id);
        std::fs::write(
            &path,
            "#!/usr/bin/env bash\n# period: 1h\n# repeat: 1m\nexit 0\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        let cfg = Config {
            checks_dir: checks_dir.clone(),
            state_dir: Some(root.join("state")),
            libexec_dir: None,
            telegram: None,
            quiet_time: quiet.then(quiet_now_config),
            defaults: crate::config::Defaults::default(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut engine = Engine::new(&cfg, tx).unwrap();
        engine.scan_dir(&checks_dir);
        (engine, id, root)
    }

    /// run the check once and report the outcome as if it had just finished
    fn finish(engine: &mut Engine, id: &str, outcome: RunOutcome) {
        engine.spawn_run(id);
        let gen = engine.checks.get(id).unwrap().running_gen.unwrap();
        engine.handle_done(id, gen, outcome);
    }

    /// a failure inside the quiet hours is held back, survives a restart, and
    /// goes out when the window ends
    #[tokio::test]
    async fn quiet_hours_hold_an_alert_until_the_window_ends() {
        let (mut engine, id, root) = engine_with_check("quiet-alert", true);
        finish(&mut engine, &id, failed());

        let state = engine.states.get(&id).unwrap();
        assert!(state.alert_active);
        assert!(state.deferred_alert.is_some());
        assert_eq!(state.alert_count, 0, "nothing has been sent yet");
        assert!(
            state.last_alert_at.is_none(),
            "the escalation must not start before the delivery"
        );

        // parked on disk too, or a restart at 3am would lose the alert
        let persisted = StateStore::new(root.join("state")).load_all().unwrap();
        assert!(persisted.get(&id).unwrap().deferred_alert.is_some());

        engine.flush_deferred();
        let state = engine.states.get(&id).unwrap();
        assert!(state.deferred_alert.is_none());
        assert_eq!(state.alert_count, 1, "the summary counts as the delivery");
        assert!(state.last_alert_at.is_some());
        let persisted = StateStore::new(root.join("state")).load_all().unwrap();
        assert!(persisted.get(&id).unwrap().deferred_alert.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// a failure and its recovery both inside the window: the user was never
    /// told about the incident, so the morning brings nothing at all
    #[tokio::test]
    async fn quiet_hours_swallow_a_failure_that_recovers_inside_the_window() {
        let (mut engine, id, root) = engine_with_check("quiet-recover", true);
        finish(&mut engine, &id, failed());
        assert!(engine.states.get(&id).unwrap().deferred_alert.is_some());

        finish(&mut engine, &id, ok());
        engine.flush_deferred();

        let state = engine.states.get(&id).unwrap();
        assert!(!state.alert_active);
        assert!(state.deferred_alert.is_none());
        assert!(state.deferred_restored.is_none(), "the alert never went out");
        assert_eq!(state.alert_count, 0);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// the alert went out before the window: its restore waits for the morning
    /// instead of being swallowed along with the night's noise
    #[tokio::test]
    async fn quiet_hours_defer_the_restore_of_an_earlier_alert() {
        let (mut engine, id, root) = engine_with_check("quiet-restored", false);
        finish(&mut engine, &id, failed());
        assert_eq!(engine.states.get(&id).unwrap().alert_count, 1, "sent right away");

        // the window opens, then the check recovers
        engine.quiet = QuietWindow::parse(Some(&quiet_now_config())).unwrap();
        finish(&mut engine, &id, ok());
        assert!(engine.states.get(&id).unwrap().deferred_restored.is_some());

        engine.flush_deferred();
        assert!(engine.states.get(&id).unwrap().deferred_restored.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// a repeat that comes due overnight is not lost either: it joins the
    /// summary, and the escalation advances only on that delivery
    #[tokio::test]
    async fn quiet_hours_hold_a_repeat_and_escalate_on_delivery() {
        let (mut engine, id, root) = engine_with_check("quiet-repeat", false);
        finish(&mut engine, &id, failed());
        assert_eq!(engine.states.get(&id).unwrap().alert_count, 1);

        // pretend the alert went out long ago, so the repeat is due now
        engine.states.get_mut(&id).unwrap().last_alert_at = Some(epoch_now() - 3600);
        engine.quiet = QuietWindow::parse(Some(&quiet_now_config())).unwrap();
        finish(&mut engine, &id, failed());

        let state = engine.states.get(&id).unwrap();
        assert!(state.deferred_alert.is_some(), "the repeat waits");
        assert!(state.deferred_alert_repeat);
        assert_eq!(state.repeat_index, 0, "the step advances on delivery only");

        engine.flush_deferred();
        let state = engine.states.get(&id).unwrap();
        assert_eq!(state.repeat_index, 1);
        assert_eq!(state.alert_count, 2);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// the flush happens when the window closes, not on every tick: while it is
    /// still open, a passing second must leave the parked messages parked
    #[tokio::test]
    async fn check_quiet_window_flushes_only_after_the_window() {
        let (mut engine, id, root) = engine_with_check("quiet-edge", true);
        finish(&mut engine, &id, failed());

        engine.check_quiet_window();
        assert!(engine.in_quiet);
        assert!(
            engine.states.get(&id).unwrap().deferred_alert.is_some(),
            "still inside the window: nothing may go out"
        );

        // same parked state, but the window is over now
        engine.quiet = QuietWindow::parse(Some(&quiet_later_config())).unwrap();
        engine.check_quiet_window();
        assert!(!engine.in_quiet, "the fresh engine sees the window closed");
        let state = engine.states.get(&id).unwrap();
        assert!(state.deferred_alert.is_none());
        assert_eq!(state.alert_count, 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    /// a restart inside the window loads the parked messages and does not
    /// flush them early — without the state file the alert would be gone
    #[tokio::test]
    async fn a_restart_inside_the_window_keeps_the_parked_alert() {
        use tokio::sync::mpsc;

        let (mut engine, id, root) = engine_with_check("quiet-restart", true);
        finish(&mut engine, &id, failed());
        drop(engine);

        let cfg = Config {
            checks_dir: root.join("checks"),
            state_dir: Some(root.join("state")),
            libexec_dir: None,
            telegram: None,
            quiet_time: Some(quiet_now_config()),
            defaults: crate::config::Defaults::default(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut restarted = Engine::new(&cfg, tx).unwrap();
        restarted.set_states(StateStore::new(root.join("state")).load_all().unwrap());
        restarted.check_quiet_window();

        let state = restarted.states.get(&id).unwrap();
        assert!(state.deferred_alert.is_some(), "the parked alert survived");
        assert_eq!(state.alert_count, 0, "and it is still undelivered");
        assert!(restarted.in_quiet, "the new process knows it is quiet");
        assert!(restarted.quiet_now());

        // a daemon that was down across the window end delivers in the morning,
        // on the very first tick
        let closed = Config {
            checks_dir: root.join("checks"),
            state_dir: Some(root.join("state")),
            libexec_dir: None,
            telegram: None,
            quiet_time: Some(quiet_later_config()),
            defaults: crate::config::Defaults::default(),
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut morning = Engine::new(&closed, tx).unwrap();
        morning.set_states(StateStore::new(root.join("state")).load_all().unwrap());
        morning.check_quiet_window();

        let state = morning.states.get(&id).unwrap();
        assert!(state.deferred_alert.is_none(), "delivered at the first tick");
        assert_eq!(state.alert_count, 1);
        let persisted = StateStore::new(root.join("state")).load_all().unwrap();
        assert!(persisted.get(&id).unwrap().deferred_alert.is_none());
        let _ = std::fs::remove_dir_all(&root);
    }

    /// a restore parked earlier in the window and a failure parked later both
    /// go out in the same summary
    #[tokio::test]
    async fn quiet_hours_report_a_parked_restore_and_a_later_failure_together() {
        let (mut engine, id, root) = engine_with_check("quiet-mixed", false);
        finish(&mut engine, &id, failed()); // told about it before the window
        engine.quiet = QuietWindow::parse(Some(&quiet_now_config())).unwrap();
        finish(&mut engine, &id, ok()); // recovered inside the window
        finish(&mut engine, &id, failed()); // and failed again inside it

        let state = engine.states.get(&id).unwrap();
        assert!(state.deferred_restored.is_some(), "the restore waits");
        assert!(state.deferred_alert.is_some(), "the new failure waits too");

        engine.flush_deferred();
        let state = engine.states.get(&id).unwrap();
        assert!(state.deferred_restored.is_none());
        assert!(state.deferred_alert.is_none());
        assert_eq!(state.alert_count, 2, "the bedtime alert and the summary");
        assert_eq!(state.repeat_index, 0, "not a repeat: the escalation starts fresh");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// a held-back repeat is dropped when the check recovers, but the restore
    /// still goes out: the incident itself was announced before the window, so
    /// swallowing it would leave it looking down
    #[tokio::test]
    async fn quiet_hours_keep_the_restore_of_a_held_back_repeat() {
        let (mut engine, id, root) = engine_with_check("quiet-repeat-restore", false);
        finish(&mut engine, &id, failed()); // announced before the window
        engine.states.get_mut(&id).unwrap().last_alert_at = Some(epoch_now() - 3600);

        engine.quiet = QuietWindow::parse(Some(&quiet_now_config())).unwrap();
        finish(&mut engine, &id, failed()); // the repeat is held back
        assert!(engine.states.get(&id).unwrap().deferred_alert_repeat);

        finish(&mut engine, &id, ok()); // and recovers inside the window
        let state = engine.states.get(&id).unwrap();
        assert!(!state.alert_active);
        assert!(state.deferred_alert.is_none(), "the repeat is obsolete");
        assert!(
            state.deferred_restored.is_some(),
            "the incident was announced, so it must be closed out"
        );
        let _ = std::fs::remove_dir_all(&root);
    }

    /// `# report_restored: false` stays silent as usual, even when the alert
    /// itself went out before the window
    #[tokio::test]
    async fn quiet_hours_respect_report_restored_false() {
        let (mut engine, id, root) = engine_with_check("quiet-norestore", false);
        engine.checks.get_mut(&id).unwrap().meta.report_restored = false;
        finish(&mut engine, &id, failed());

        engine.quiet = QuietWindow::parse(Some(&quiet_now_config())).unwrap();
        finish(&mut engine, &id, ok());
        engine.flush_deferred();

        let state = engine.states.get(&id).unwrap();
        assert!(!state.alert_active);
        assert!(state.deferred_restored.is_none());
        assert!(state.deferred_alert.is_none());
        assert_eq!(state.alert_count, 1);
        let _ = std::fs::remove_dir_all(&root);
    }
}
