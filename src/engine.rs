use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
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
            FsAction::Upsert(path) => {
                tracing::debug!("fs event: upsert {}", path.display());
                self.upsert(&path)
            }
            FsAction::Remove(path) => {
                tracing::debug!("fs event: remove {}", path.display());
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

        // remember the run start so the schedule survives restarts
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
                if meta.report_restored {
                    let name = meta.name.as_deref().unwrap_or(id);
                    let after = match downtime {
                        Some(d) => format!(" after {}", fmt_human(d)),
                        None => String::new(),
                    };
                    self.send_notify(id, format!("🟢 {name}: restored{after}"));
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
                    state.alert_count += 1;
                    state.last_alert_at = Some(epoch_now());
                    tracing::warn!(
                        "check failed: {id} (code={:?} timed_out={})",
                        outcome.code,
                        outcome.timed_out
                    );
                    self.send_notify(id, format_alert(id, &meta, &outcome));
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
    format!(
        "{}\n(down for {})",
        format_alert(id, meta, outcome),
        fmt_human(down)
    )
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

    #[test]
    fn repeat_alert_appends_downtime() {
        let meta = Meta::parse("# name: web\n");
        assert_eq!(
            format_repeat_alert("web.check.sh", &meta, &failed(), Duration::from_secs(9000)),
            "🔴 web: check failed (exit=1)\n(down for 2h 30m)"
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

    #[test]
    fn fmt_human_matches_config_style() {
        assert_eq!(fmt_human(Duration::from_secs(5)), "5s");
        assert_eq!(fmt_human(Duration::from_secs(90)), "1m 30s");
        assert_eq!(fmt_human(Duration::from_secs(300)), "5m");
        assert_eq!(fmt_human(Duration::from_secs(3725)), "1h 2m 5s");
        assert_eq!(fmt_human(Duration::from_secs(90000)), "1d 1h");
        assert_eq!(fmt_human(Duration::from_millis(240)), "0s");
    }
}
