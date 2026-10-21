use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::mpsc;
use tracing::{debug, info};
use uuid::Uuid;

use crate::client::{ClientError, DebugClient};
use crate::lease::{Lease, Reclaim};
use crate::listener::EventListener;
use crate::module_index::ModuleIndex;
use crate::types::base::{CalcPathItem, ModuleId, StepAction, ViewInterface};
use crate::types::events::DebugEvent;
use crate::types::responses::{self, AttachResult, EvalPayload};
use crate::types::xml::BreakpointDef;

pub struct DebugSession {
    client: Arc<DebugClient>,
    listener: EventListener,
    events: mpsc::UnboundedReceiver<DebugEvent>,
    index: ModuleIndex,
    attached_targets: HashMap<String, String>,
    auto_attach: Vec<String>,
    breakpoints: Vec<BreakpointDef>,
    /// The target commands act on.
    stopped_target: Option<String>,
    /// Client and server can be stopped at once, so one slot cannot tell whether the target a
    /// command is about to reach is still halted. Ordered by stop, latest last: once the target
    /// of the latest stop resumes, commands fall back to the one that stopped before it.
    stopped_targets: Vec<String>,
    /// Stops drained from the event queue but not yet returned by `wait_for_stop`, one per
    /// target, latest last. Two targets can stop between two waits, and the earlier one is still
    /// halted: dropping its stop would leave it with no id the caller could ever see.
    unreported_stops: Vec<StopEvent>,
    /// Sent a step and not stopped since. `stopByBP` cannot tell a step's end from a breakpoint,
    /// so the step itself is the record to classify their next stop by.
    steps_in_flight: Vec<StepInFlight>,
    /// Call-stack depth of each halted target at its stop, where the stop carried a stack.
    stop_depths: HashMap<String, usize>,
    last_stop: Option<StopEvent>,
    watches: Vec<String>,
    pending_eval_results: HashMap<String, Vec<u8>>,
    /// The evaluation whose answer is being waited for. A result for any other id, including one
    /// whose wait already ended, is not stored: nothing would ever remove it.
    waiting_eval: Option<String>,
    /// Refusals of requests the event handler makes on its own. They are not stops, so they must
    /// not abort `wait_for_stop`; the caller still has to hear about them.
    runtime_warnings: Vec<String>,
    /// Listed once while connecting, because a second `getDbgTargets` can answer differently and
    /// its failure must not undo an attach that already happened.
    known_targets: Vec<TargetInfo>,
    setup_warnings: Vec<String>,
    /// Halted before this session attached and not yet reported by `wait_for_stop`: the server
    /// never replays their stop event, so nothing else would ever announce them.
    pending_suspended: VecDeque<String>,
    suspended_at_attach: Vec<String>,
    lease: Option<Lease>,
    reclaimed_dead_session: bool,
}

/// The target types a front end attaches when its caller names none. A FILE infobase produces
/// `ManagedClient` and `ServerEmulation` and nothing else, so leaving those two out makes
/// file-mode debugging attach nothing at all.
pub const DEFAULT_AUTO_ATTACH: &[&str] =
    &["Client", "ManagedClient", "Server", "ServerEmulation", "HTTPService"];

pub struct DebugConfig {
    pub host: String,
    pub port: u16,
    pub infobase: String,
    pub config_root: PathBuf,
    pub extensions: Vec<(String, PathBuf)>,
    pub auto_attach: Vec<String>,
    /// Where the debugger UI id of a live session is recorded, so that a session started after
    /// this one died can take its registration over (see [`crate::lease`]). `None` disables both
    /// the record and the takeover.
    pub lease_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StopEvent {
    pub reason: StopReason,
    pub target_id: String,
    pub module: String,
    pub line: u32,
    pub stack: Vec<FrameInfo>,
    /// Why the position is unknown, when it is.
    pub detail: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub enum StopReason {
    Breakpoint,
    Step,
    Exception {
        message: String,
    },
    /// Halted before this session attached — typically by a debugger that went away without
    /// resuming it. Its breakpoint or step is not known to this session.
    Suspended,
}

/// What `disconnect` did besides detaching.
#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct DisconnectReport {
    /// Targets resumed because they were halted: once the debugger is gone nothing else would
    /// resume them, and their client stays frozen.
    pub resumed: Vec<String>,
    pub warnings: Vec<String>,
    /// Why `detachDebugUI` failed, when it did. The targets above were resumed regardless, and
    /// the caller has to hear about both.
    pub detach_error: Option<String>,
}

/// A step sent to a target, kept until the target's next stop.
#[derive(Debug, Clone)]
struct StepInFlight {
    target_id: String,
    action: StepAction,
    /// Call-stack depth where the step started, when the stop it started from carried a stack.
    start_depth: Option<usize>,
}

impl StepInFlight {
    /// Whether a stop at `depth` lies deeper than this step can end, so a breakpoint, not the
    /// step, stopped the target there. Without both depths the stop is taken as the step's end.
    fn overshot(&self, depth: usize) -> bool {
        let Some(start) = self.start_depth else { return false };
        if depth == 0 {
            return false;
        }
        match self.action {
            StepAction::Next => depth > start,
            StepAction::StepIn => depth > start + 1,
            StepAction::StepOut => depth >= start,
            StepAction::Continue => true,
        }
    }
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct FrameInfo {
    pub module_path: String,
    pub module_name: String,
    pub line: u32,
    pub presentation: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Variable {
    pub name: String,
    pub type_name: String,
    pub value: String,
    pub expandable: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct EvalResult {
    pub value: String,
    pub type_name: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TargetInfo {
    pub id: String,
    pub target_type: String,
    pub user_name: String,
    pub infobase_alias: String,
    pub state: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct WatchResult {
    pub expression: String,
    pub value: Option<EvalResult>,
    pub error: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SourceLine {
    pub line: u32,
    pub text: String,
    pub is_current: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct StopSnapshot {
    pub reason: StopReason,
    pub target_id: String,
    pub module: String,
    pub line: u32,
    pub source_context: Vec<SourceLine>,
    pub stack: Vec<FrameInfo>,
    pub locals: Vec<Variable>,
    pub watches: Vec<WatchResult>,
}

impl DebugSession {
    pub fn connect(config: DebugConfig) -> Result<Self, ClientError> {
        let debugger_id = Uuid::new_v4().to_string();

        let ext_refs: Vec<(&str, &Path)> =
            config.extensions.iter().map(|(n, p)| (n.as_str(), p.as_path())).collect();
        let index =
            ModuleIndex::scan(&config.config_root, &ext_refs).map_err(ClientError::Debug)?;
        info!(modules = index.len(), "module index built");

        let lease_name = Lease::file_name(&config.host, config.port, &config.infobase);
        let mut setup_warnings: Vec<String> = Vec::new();
        let mut client =
            DebugClient::new(&config.host, config.port, &debugger_id, &config.infobase);
        let mut lease = None;
        let mut reclaimed_dead_session = false;

        match client.attach()? {
            AttachResult::Ok => {
                info!("attached to debug server");
                if let Some(dir) = &config.lease_dir {
                    match Lease::acquire(dir, &lease_name, &debugger_id) {
                        Ok(Some(held)) => lease = Some(held),
                        Ok(None) => setup_warnings.push(
                            "another live session of this user holds the debugger lease for this \
                             endpoint; if this session dies without disconnect, a later one \
                             cannot take its place"
                                .into(),
                        ),
                        Err(e) => setup_warnings.push(format!("debugger lease not recorded: {e}")),
                    }
                }
            }
            // Proceeding here used to look like a working session: every command after it is
            // refused, and the event channel answers "UI+ - часть отладки не зарегистрирована",
            // so no stop could ever arrive.
            AttachResult::IBInDebug => {
                let (held, previous_id) =
                    reclaim_dead_session(config.lease_dir.as_deref(), &lease_name)?;
                client =
                    DebugClient::new(&config.host, config.port, &previous_id, &config.infobase);
                match client.attach()? {
                    AttachResult::Ok => {
                        info!(previous_id, "took over the registration of a dead session");
                        lease = Some(held);
                        reclaimed_dead_session = true;
                    }
                    other => {
                        // The holder is not the session the lease remembers, so the lease
                        // describes nothing any more.
                        held.release();
                        return Err(ClientError::AttachFailed(format!(
                            "{}; the id left by a previous session of this user was refused too \
                             ({}), so the holder is someone else",
                            in_debug_message(),
                            other.error_message().unwrap_or("registered")
                        )));
                    }
                }
            }
            other => {
                return Err(ClientError::AttachFailed(
                    other.error_message().unwrap_or("unknown error").to_string(),
                ));
            }
        }
        let client = Arc::new(client);

        // Both are conveniences the session works without, and the HTTP status of neither used to
        // be read at all: a server that refuses one must not now cost the caller the whole attach.
        if let Err(e) = client.init_settings() {
            setup_warnings.push(format!("initSettings refused: {e}"));
        }

        if !config.auto_attach.is_empty() {
            let types: Vec<&str> = config.auto_attach.iter().map(|s| s.as_str()).collect();
            if let Err(e) = client.set_auto_attach(&types) {
                setup_warnings.push(format!("setAutoAttachSettings refused: {e}"));
            }
        }

        let (listener, events) = EventListener::start(client.clone(), 50);

        let mut session = Self {
            client,
            listener,
            events,
            index,
            attached_targets: HashMap::new(),
            auto_attach: config.auto_attach,
            breakpoints: Vec::new(),
            stopped_target: None,
            stopped_targets: Vec::new(),
            unreported_stops: Vec::new(),
            steps_in_flight: Vec::new(),
            stop_depths: HashMap::new(),
            last_stop: None,
            watches: Vec::new(),
            pending_eval_results: HashMap::new(),
            waiting_eval: None,
            runtime_warnings: Vec::new(),
            known_targets: Vec::new(),
            setup_warnings,
            pending_suspended: VecDeque::new(),
            suspended_at_attach: Vec::new(),
            lease,
            reclaimed_dead_session,
        };

        // `setAutoAttachSettings` only covers targets that start later, so a client that was
        // already running would never be attached and no breakpoint would ever be hit.
        session.attach_running_targets();
        session.adopt_suspended_targets();

        Ok(session)
    }

    /// A target halted before this attach stays halted, and the server does not repeat its stop
    /// event to a new debugger: unless it is found here, `wait_stop` times out forever and
    /// `continue` has no target to act on, while the client stays frozen.
    fn adopt_suspended_targets(&mut self) {
        let states = match self.client.get_all_target_states() {
            Ok(states) => states,
            Err(e) => {
                self.setup_warnings.push(format!(
                    "getDbgAllTargetStates refused: {e}; a target halted before this attach \
                     would go unnoticed"
                ));
                return;
            }
        };
        for state in states {
            if let Some(known) = self.known_targets.iter_mut().find(|k| k.id == state.id) {
                known.state = state.state.clone();
            }
            if !responses::is_suspended_state(&state.state) {
                continue;
            }
            if !self.attached_targets.contains_key(&state.id) {
                let why = if self.auto_attach_allows(&state.target_type) {
                    "attaching it failed"
                } else {
                    "auto_attach leaves it out"
                };
                self.setup_warnings.push(format!(
                    "target {} ({}) is halted, but {why}, so this session cannot resume it",
                    state.id, state.target_type
                ));
                continue;
            }
            info!(target_id = %state.id, state = %state.state, "target was halted before attach");
            self.mark_stopped(&state.id);
            if self.stopped_target.is_none() {
                self.stopped_target = Some(state.id.clone());
            }
            self.pending_suspended.push_back(state.id.clone());
            self.suspended_at_attach.push(state.id);
        }
    }

    fn attach_running_targets(&mut self) {
        let targets = match self.client.get_targets() {
            Ok(targets) => targets,
            Err(e) => {
                tracing::warn!(error = %e, "could not list running debug targets");
                self.setup_warnings.push(format!("getDbgTargets refused: {e}"));
                return;
            }
        };

        self.known_targets = targets
            .iter()
            .map(|t| TargetInfo {
                id: t.id.clone(),
                target_type: t.target_type.clone(),
                user_name: t.user_name.clone(),
                infobase_alias: t.infobase_alias.clone(),
                state: t.state.clone(),
            })
            .collect();

        for target in targets {
            if !self.auto_attach_allows(&target.target_type) {
                debug!(
                    target_id = %target.id,
                    target_type = %target.target_type,
                    "running target skipped by auto_attach"
                );
                continue;
            }
            match self.client.attach_targets(&[&target.id]) {
                Ok(()) => {
                    info!(target_id = %target.id, target_type = %target.target_type,
                        "attached to a running target");
                    self.attached_targets.insert(target.id, target.target_type);
                }
                Err(e) => tracing::warn!(target_id = %target.id, error = %e,
                    "could not attach a running debug target"),
            }
        }
    }

    /// An empty `auto_attach` attaches everything; target types are compared case-insensitively
    /// because they come back from the server, not from our own table.
    fn auto_attach_allows(&self, target_type: &str) -> bool {
        self.auto_attach.is_empty()
            || self.auto_attach.iter().any(|t| t.eq_ignore_ascii_case(target_type))
    }

    pub fn attached_count(&self) -> usize {
        self.attached_targets.len()
    }

    pub fn known_targets(&self) -> &[TargetInfo] {
        &self.known_targets
    }

    /// Non-fatal refusals collected during the handshake, so `attach` can show them instead of
    /// leaving the caller with a session that is quietly less capable than it looks.
    pub fn setup_warnings(&self) -> &[String] {
        &self.setup_warnings
    }

    /// Targets found halted when this session attached.
    pub fn suspended_at_attach(&self) -> &[String] {
        &self.suspended_at_attach
    }

    /// Whether this session re-registered under the id of a session that died holding the
    /// infobase.
    pub fn reclaimed_dead_session(&self) -> bool {
        self.reclaimed_dead_session
    }

    pub fn lease_path(&self) -> Option<&Path> {
        self.lease.as_ref().map(|l| l.path())
    }

    /// Why polling the event channel fails, while it does.
    pub fn event_channel_error(&self) -> Option<String> {
        self.listener.last_error()
    }

    /// Refusals of requests this session made while handling events, oldest first. Taking them
    /// clears the list, so each refusal is reported once.
    pub fn take_runtime_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.runtime_warnings)
    }

    pub fn disconnect(mut self) -> DisconnectReport {
        let mut report = DisconnectReport::default();

        // Cleared first: a target resumed below would otherwise halt on one of these again, with
        // no debugger left to resume it.
        if !self.breakpoints.is_empty() {
            if let Err(e) = self.client.set_breakpoints(&[]) {
                report.warnings.push(format!("breakpoints not cleared: {e}"));
            }
            self.breakpoints.clear();
        }

        // A target left halted keeps its client frozen after the debugger is gone. The server's
        // own state list is the authority; the session's set is only a fallback, since a target
        // in it may have been resumed by someone else meanwhile.
        let halted: Vec<String> = match self.client.get_all_target_states() {
            Ok(states) => states
                .into_iter()
                .filter(|s| {
                    responses::is_suspended_state(&s.state)
                        && self.attached_targets.contains_key(&s.id)
                })
                .map(|s| s.id)
                .collect(),
            Err(e) => {
                report.warnings.push(format!("getDbgAllTargetStates refused: {e}"));
                self.stopped_targets.clone()
            }
        };
        for id in halted {
            match self.client.step(&id, "Continue") {
                Ok(()) => report.resumed.push(id),
                Err(e) => report.warnings.push(format!("target {id} not resumed: {e}")),
            }
        }

        self.listener.stop();
        for id in self.attached_targets.keys() {
            if let Err(e) = self.client.detach_targets(&[id.as_str()]) {
                debug!(target_id = %id, error = %e, "detaching a target on disconnect failed");
            }
        }
        // On failure the lease stays: the registration may still be alive, and a later session
        // can then take it over.
        if let Err(e) = self.client.detach() {
            report.detach_error = Some(e.to_string());
            return report;
        }
        if let Some(lease) = self.lease.take() {
            lease.release();
        }
        info!(resumed = report.resumed.len(), "detached from debug server");
        report
    }

    pub fn set_breakpoint(
        &mut self,
        module_name: &str,
        line: u32,
        condition: Option<&str>,
    ) -> Result<(), ClientError> {
        let (module_id, _path) = self.index.resolve_name(module_name).ok_or_else(|| {
            ClientError::Debug(crate::error::DebugError::ModuleNotFound(module_name.to_string()))
        })?;

        self.breakpoints.push(BreakpointDef {
            extension: module_id.extension.clone(),
            object_id: module_id.object_id.clone(),
            property_id: module_id.property_id.clone(),
            line,
            condition: condition.map(|s| s.to_string()),
        });

        // A rejected list must not leave a phantom breakpoint behind: every later
        // `setBreakpoints` sends the whole list and would be rejected for the same reason.
        if let Err(e) = self.client.set_breakpoints(&self.breakpoints) {
            self.breakpoints.pop();
            return Err(e);
        }
        info!(module_name, line, "breakpoint set");
        Ok(())
    }

    pub fn set_breakpoint_by_path(
        &mut self,
        path: &Path,
        line: u32,
        condition: Option<&str>,
    ) -> Result<(), ClientError> {
        let module_id = self.index.module_by_path(path).ok_or_else(|| {
            ClientError::Debug(crate::error::DebugError::ModuleNotFound(path.display().to_string()))
        })?;

        self.breakpoints.push(BreakpointDef {
            extension: module_id.extension.clone(),
            object_id: module_id.object_id.clone(),
            property_id: module_id.property_id.clone(),
            line,
            condition: condition.map(|s| s.to_string()),
        });

        if let Err(e) = self.client.set_breakpoints(&self.breakpoints) {
            self.breakpoints.pop();
            return Err(e);
        }
        Ok(())
    }

    pub fn remove_breakpoint(&mut self, module_name: &str, line: u32) -> Result<bool, ClientError> {
        let (module_id, _path) = match self.index.resolve_name(module_name) {
            Some(v) => v,
            None => return Ok(false),
        };
        let before = self.breakpoints.clone();
        self.breakpoints.retain(|bp| {
            !(bp.object_id == module_id.object_id
                && bp.property_id == module_id.property_id
                && bp.extension == module_id.extension
                && bp.line == line)
        });
        if self.breakpoints.len() != before.len() {
            if let Err(e) = self.client.set_breakpoints(&self.breakpoints) {
                self.breakpoints = before;
                return Err(e);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn remove_breakpoint_by_path(
        &mut self,
        path: &Path,
        line: u32,
    ) -> Result<bool, ClientError> {
        let module_id = match self.index.module_by_path(path) {
            Some(v) => v,
            None => return Ok(false),
        };
        let before = self.breakpoints.clone();
        self.breakpoints.retain(|bp| {
            !(bp.object_id == module_id.object_id
                && bp.property_id == module_id.property_id
                && bp.extension == module_id.extension
                && bp.line == line)
        });
        if self.breakpoints.len() != before.len() {
            if let Err(e) = self.client.set_breakpoints(&self.breakpoints) {
                self.breakpoints = before;
                return Err(e);
            }
            Ok(true)
        } else {
            Ok(false)
        }
    }

    pub fn clear_breakpoints(&mut self) -> Result<(), ClientError> {
        let before = std::mem::take(&mut self.breakpoints);
        if let Err(e) = self.client.set_breakpoints(&self.breakpoints) {
            self.breakpoints = before;
            return Err(e);
        }
        Ok(())
    }

    pub fn break_on_error(&self, filter: Option<&str>) -> Result<(), ClientError> {
        self.client.set_break_on_error(true, filter)
    }

    pub fn ignore_errors(&self) -> Result<(), ClientError> {
        self.client.set_break_on_error(false, None)
    }

    pub fn module_count(&self) -> usize {
        self.index.len()
    }

    pub fn module_names(&self) -> Vec<&str> {
        self.index.all_names()
    }

    pub fn targets(&self) -> Result<Vec<TargetInfo>, ClientError> {
        let targets = self.client.get_targets()?;
        Ok(targets
            .into_iter()
            .map(|t| TargetInfo {
                id: t.id,
                target_type: t.target_type,
                user_name: t.user_name,
                infobase_alias: t.infobase_alias,
                state: t.state,
            })
            .collect())
    }

    pub fn stopped_target(&self) -> Option<&str> {
        self.stopped_target.as_deref()
    }

    pub fn stopped_count(&self) -> usize {
        self.stopped_targets.len()
    }

    pub fn target_type_of(&self, id: &str) -> Option<&str> {
        self.attached_targets.get(id).map(|t| t.as_str())
    }

    pub fn last_stop(&self) -> Option<&StopEvent> {
        self.last_stop.as_ref()
    }

    pub fn wait_for_stop(&mut self, timeout: Duration) -> Result<Option<StopEvent>, ClientError> {
        while let Ok(event) = self.events.try_recv() {
            if let Some(stop) = self.handle_event(event)? {
                self.queue_stop(stop);
            }
        }
        // The freshest stop goes first. A stop of a target that has resumed since describes a
        // position the caller is already past, and commands would act somewhere else entirely.
        self.unreported_stops.retain(|s| self.stopped_targets.contains(&s.target_id));
        if let Some(stop) = self.unreported_stops.pop() {
            return Ok(Some(self.report_stop(stop)));
        }

        while let Some(target_id) = self.pending_suspended.pop_front() {
            // Resumed meanwhile — by this session's `continue` or by the target ending.
            if !self.stopped_targets.contains(&target_id) {
                continue;
            }
            let stop = self.suspended_stop(target_id, timeout);
            self.last_stop = Some(stop.clone());
            return Ok(Some(stop));
        }

        let deadline = std::time::Instant::now() + timeout;

        loop {
            match self.events.try_recv() {
                Ok(event) => {
                    if let Some(stop) = self.handle_event(event)? {
                        return Ok(Some(self.report_stop(stop)));
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        return Ok(None);
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Ok(None);
                }
            }
        }
    }

    /// A newer stop of the same target supersedes the queued one.
    fn queue_stop(&mut self, stop: StopEvent) {
        self.unreported_stops.retain(|s| s.target_id != stop.target_id);
        self.unreported_stops.push(stop);
    }

    fn report_stop(&mut self, stop: StopEvent) -> StopEvent {
        self.unreported_stops.retain(|s| s.target_id != stop.target_id);
        self.stopped_target = Some(stop.target_id.clone());
        self.last_stop = Some(stop.clone());
        stop
    }

    fn mark_stopped(&mut self, target_id: &str) {
        self.stopped_targets.retain(|t| t != target_id);
        self.stopped_targets.push(target_id.to_string());
    }

    /// Records a stop event of the server and hands back the step this session sent to the
    /// target, if one was in flight. The event describes this halt itself, so an announcement of
    /// the same target as halted before the attach would report it a second time and is dropped.
    fn record_stop_event(&mut self, target_id: &str, depth: Option<usize>) -> Option<StepInFlight> {
        self.mark_stopped(target_id);
        self.stopped_target = Some(target_id.to_string());
        self.pending_suspended.retain(|t| t != target_id);
        self.record_depth(target_id, depth);
        let at = self.steps_in_flight.iter().position(|s| s.target_id == target_id)?;
        Some(self.steps_in_flight.remove(at))
    }

    fn record_depth(&mut self, target_id: &str, depth: Option<usize>) {
        match depth {
            Some(depth) => self.stop_depths.insert(target_id.to_string(), depth),
            None => self.stop_depths.remove(target_id),
        };
    }

    /// The target is running again or gone; commands that named no target fall back to the
    /// target that stopped most recently among those still halted.
    fn mark_resumed(&mut self, target_id: &str) {
        self.stopped_targets.retain(|t| t != target_id);
        self.pending_suspended.retain(|t| t != target_id);
        self.unreported_stops.retain(|s| s.target_id != target_id);
        self.steps_in_flight.retain(|s| s.target_id != target_id);
        self.stop_depths.remove(target_id);
        if self.stopped_target.as_deref() == Some(target_id) {
            self.stopped_target = self.stopped_targets.last().cloned();
        }
    }

    /// Where a target halted before this attach stands. Only `getCallStack` can say: the stop
    /// event that carried the position went to the debugger that halted it.
    ///
    /// The wait for that answer is the caller's `wait_stop` budget, capped: the halt itself is
    /// already known, and only its position is being waited for. A caller that gives no time at
    /// all gets the halt at once, without the position.
    fn suspended_stop(&mut self, target_id: String, budget: Duration) -> StopEvent {
        self.stopped_target = Some(target_id.clone());
        let wait = budget.min(crate::client::CALL_STACK_TIMEOUT);
        let answer = if wait.is_zero() {
            Err("call stack not requested: wait_stop was given no time to wait for it; \
                 stack_trace asks for it, and locals, eval and continue work regardless"
                .to_string())
        } else {
            self.call_stack_within(&target_id, wait).map_err(|e| {
                format!("call stack unavailable: {e}; locals, eval and continue work regardless")
            })
        };
        let (stack, detail) = match answer {
            Ok(stack) if !stack.is_empty() => (stack, None),
            Ok(_) => (
                Vec::new(),
                Some(
                    "the debug server answered the call-stack request with nothing (both 8.3.17 \
                     and 8.3.27 do so at times); locals, eval and continue work regardless"
                        .to_string(),
                ),
            ),
            Err(detail) => (Vec::new(), Some(detail)),
        };
        self.record_depth(&target_id, (!stack.is_empty()).then_some(stack.len()));
        let (module, line) = stack
            .first()
            .map(|f| {
                let module = if f.module_path.is_empty() {
                    f.module_name.clone()
                } else {
                    f.module_path.clone()
                };
                (module, f.line)
            })
            .unwrap_or_default();
        StopEvent { reason: StopReason::Suspended, target_id, module, line, stack, detail }
    }

    pub fn step(&mut self, action: StepAction) -> Result<(), ClientError> {
        self.step_target(action, None)
    }

    /// `target` picks one of the halted targets explicitly; without it the command goes to the
    /// target of the latest stop. Two targets can be halted at once — one left over from before
    /// the attach and one this session stopped — and only an id tells them apart.
    pub fn step_target(
        &mut self,
        action: StepAction,
        target: Option<&str>,
    ) -> Result<(), ClientError> {
        let target_id = match target {
            Some(id) if self.stopped_targets.iter().any(|t| t == id) => id.to_string(),
            Some(id) => {
                let mut stopped: Vec<&str> =
                    self.stopped_targets.iter().map(|s| s.as_str()).collect();
                stopped.sort_unstable();
                return Err(ClientError::TargetNotStopped {
                    target: id.to_string(),
                    stopped: if stopped.is_empty() { "none".into() } else { stopped.join(", ") },
                });
            }
            None => self.require_stopped_target()?.to_string(),
        };

        let action_str = match action {
            StepAction::Next => "Step",
            StepAction::StepIn => "StepIn",
            StepAction::StepOut => "StepOut",
            StepAction::Continue => "Continue",
        };

        self.client.step(&target_id, action_str)?;
        let start_depth = self.stop_depths.get(&target_id).copied();
        self.mark_resumed(&target_id);
        if !matches!(action, StepAction::Continue) {
            self.steps_in_flight.push(StepInFlight { target_id, action, start_depth });
        }
        Ok(())
    }

    pub fn continue_execution(&mut self) -> Result<(), ClientError> {
        self.step(StepAction::Continue)
    }

    pub fn continue_target(&mut self, target: Option<&str>) -> Result<(), ClientError> {
        self.step_target(StepAction::Continue, target)
    }

    /// The target commands act on, and only while it is still halted: the debug server answers
    /// HTTP 400 for an evaluation against a running target, and a stale id sends it there.
    fn require_stopped_target(&self) -> Result<&str, ClientError> {
        if let Some(ref id) = self.stopped_target {
            if self.stopped_targets.contains(id) {
                return Ok(id);
            }
        }
        Err(ClientError::NotStopped)
    }

    pub fn call_stack(&self) -> Result<Vec<FrameInfo>, ClientError> {
        let target_id = self.require_stopped_target()?;
        self.call_stack_of(target_id)
    }

    fn call_stack_of(&self, target_id: &str) -> Result<Vec<FrameInfo>, ClientError> {
        self.call_stack_within(target_id, crate::client::CALL_STACK_TIMEOUT)
    }

    fn call_stack_within(
        &self,
        target_id: &str,
        wait: Duration,
    ) -> Result<Vec<FrameInfo>, ClientError> {
        let frames = self.client.get_call_stack(target_id, wait)?;

        Ok(frames
            .into_iter()
            .map(|f| {
                let module_id = ModuleId {
                    extension: f.module_extension,
                    object_id: f.module_object_id,
                    property_id: f.module_property_id,
                };
                let module_path = self
                    .index
                    .path_by_module(&module_id)
                    .map(|p| p.display().to_string())
                    .unwrap_or_default();

                FrameInfo {
                    module_path,
                    module_name: module_id.to_string(),
                    line: f.line_no,
                    presentation: f.presentation,
                }
            })
            .collect())
    }

    pub fn locals(&mut self, stack_level: u32) -> Result<Vec<Variable>, ClientError> {
        let target_id = self.require_stopped_target()?.to_string();

        let result_id = Uuid::new_v4().to_string();
        let payload = self.client.eval_local_vars(&target_id, stack_level as i64, &result_id)?;
        let payload = self.settle_eval(payload, &result_id)?;

        if let Some(message) = payload.error {
            return Err(ClientError::EvalFailed(message));
        }

        Ok(payload
            .values
            .into_iter()
            .map(|v| Variable {
                name: v.name,
                type_name: v.type_name,
                value: v.presentation,
                expandable: v.is_expandable,
            })
            .collect())
    }

    /// An evaluation answers HTTP 200 with an empty body and reports on the ping channel, so an
    /// empty synchronous payload is a promise of a later one, not a result. No answer within the
    /// wait is an error: an empty payload is also what a frame with no variables looks like, and
    /// the two must not be the same `Ok`.
    fn settle_eval(
        &mut self,
        payload: EvalPayload,
        result_id: &str,
    ) -> Result<EvalPayload, ClientError> {
        if payload.values.is_empty() && payload.error.is_none() {
            self.wait_for_eval_result(result_id, Duration::from_secs(5))
        } else {
            Ok(payload)
        }
    }

    fn wait_for_eval_result(
        &mut self,
        result_id: &str,
        timeout: Duration,
    ) -> Result<EvalPayload, ClientError> {
        self.waiting_eval = Some(result_id.to_string());
        let result = self.poll_eval_result(result_id, timeout);
        self.waiting_eval = None;
        result
    }

    fn poll_eval_result(
        &mut self,
        result_id: &str,
        timeout: Duration,
    ) -> Result<EvalPayload, ClientError> {
        let deadline = std::time::Instant::now() + timeout;

        loop {
            if let Some(xml) = self.pending_eval_results.remove(result_id) {
                return Ok(responses::parse_eval_result(&xml));
            }

            match self.events.try_recv() {
                Ok(event) => {
                    // A stop that arrives while an evaluation is awaited is still a stop the
                    // next `wait_for_stop` must report.
                    if let Ok(Some(stop)) = self.handle_event(event) {
                        self.queue_stop(stop);
                    }
                }
                Err(mpsc::error::TryRecvError::Empty) => {
                    if std::time::Instant::now() >= deadline {
                        debug!(result_id, "eval result timeout");
                        return Err(ClientError::EvalFailed(
                            "the debug server returned no result within 5 s".into(),
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(mpsc::error::TryRecvError::Disconnected) => {
                    return Err(ClientError::EvalFailed(
                        "the event channel closed before the evaluation result arrived".into(),
                    ));
                }
            }
        }
    }

    pub fn eval(&mut self, expression: &str, stack_level: u32) -> Result<EvalResult, ClientError> {
        let target_id = self.require_stopped_target()?.to_string();

        let result_id = Uuid::new_v4().to_string();
        let payload =
            self.client.eval_expr(&target_id, expression, stack_level as i64, &result_id)?;
        let payload = self.settle_eval(payload, &result_id)?;

        if let Some(message) = payload.error {
            return Err(ClientError::EvalFailed(message));
        }

        match payload.values.into_iter().next() {
            Some(first) => Ok(EvalResult { value: first.presentation, type_name: first.type_name }),
            // A result that arrived and carried no value is not a timeout: the wait already
            // ended in `settle_eval` when nothing came back at all.
            None => Err(ClientError::EvalFailed(
                "the debug server returned an empty evaluation result".into(),
            )),
        }
    }

    pub fn expand(
        &mut self,
        path: &[CalcPathItem],
        view: ViewInterface,
        stack_level: u32,
    ) -> Result<Vec<Variable>, ClientError> {
        let target_id = self.require_stopped_target()?.to_string();

        let result_id = Uuid::new_v4().to_string();
        let payload =
            self.client.eval_expand(&target_id, path, view, stack_level as i64, &result_id)?;
        let payload = self.settle_eval(payload, &result_id)?;

        if let Some(message) = payload.error {
            return Err(ClientError::EvalFailed(message));
        }

        Ok(payload
            .values
            .into_iter()
            .map(|v| Variable {
                name: v.name,
                type_name: v.type_name,
                value: v.presentation,
                expandable: v.is_expandable,
            })
            .collect())
    }

    pub fn add_watch(&mut self, expression: &str) {
        if !self.watches.contains(&expression.to_string()) {
            self.watches.push(expression.to_string());
        }
    }

    pub fn remove_watch(&mut self, expression: &str) {
        self.watches.retain(|w| w != expression);
    }

    pub fn watch_list(&self) -> &[String] {
        &self.watches
    }

    pub fn breakpoint_count(&self) -> usize {
        self.breakpoints.len()
    }

    pub fn eval_watches(&mut self, stack_level: u32) -> Vec<WatchResult> {
        let expressions: Vec<String> = self.watches.clone();
        expressions
            .iter()
            .map(|expr| match self.eval(expr, stack_level) {
                Ok(result) => {
                    WatchResult { expression: expr.clone(), value: Some(result), error: None }
                }
                Err(e) => WatchResult {
                    expression: expr.clone(),
                    value: None,
                    error: Some(e.to_string()),
                },
            })
            .collect()
    }

    pub fn source_context(
        &self,
        file_path: &str,
        current_line: u32,
        context: u32,
    ) -> Result<Vec<SourceLine>, ClientError> {
        let path = std::path::Path::new(file_path);
        let content = std::fs::read_to_string(path)
            .map_err(|e| ClientError::Debug(crate::error::DebugError::Io(e)))?;

        let start = current_line.saturating_sub(context).max(1);
        let end = current_line + context;

        Ok(content
            .lines()
            .enumerate()
            .filter_map(|(i, text)| {
                let line_num = (i + 1) as u32;
                if line_num >= start && line_num <= end {
                    Some(SourceLine {
                        line: line_num,
                        text: text.to_string(),
                        is_current: line_num == current_line,
                    })
                } else {
                    None
                }
            })
            .collect())
    }

    pub fn snapshot_from_stop(
        &mut self,
        stop: &StopEvent,
        source_context_lines: u32,
    ) -> Result<StopSnapshot, ClientError> {
        let locals = self.locals(0).unwrap_or_default();
        let watches = self.eval_watches(0);

        let source_context = if !stop.module.is_empty() && stop.line > 0 {
            self.source_context(&stop.module, stop.line, source_context_lines).unwrap_or_default()
        } else {
            Vec::new()
        };

        Ok(StopSnapshot {
            reason: stop.reason.clone(),
            target_id: stop.target_id.clone(),
            module: stop.module.clone(),
            line: stop.line,
            source_context,
            stack: stop.stack.clone(),
            locals,
            watches,
        })
    }

    pub fn module_index(&self) -> &ModuleIndex {
        &self.index
    }

    fn breakpoint_at(&self, module_id: &ModuleId, line: u32) -> bool {
        self.breakpoints.iter().any(|bp| {
            bp.extension == module_id.extension
                && bp.object_id == module_id.object_id
                && bp.property_id == module_id.property_id
                && bp.line == line
        })
    }

    fn handle_event(&mut self, event: DebugEvent) -> Result<Option<StopEvent>, ClientError> {
        match event {
            DebugEvent::TargetStarted { target_id, target_type } => {
                if !self.auto_attach_allows(&target_type) {
                    debug!(target_id, target_type, "target skipped by auto_attach");
                    return Ok(None);
                }
                info!(target_id, target_type, "target started");
                // A refusal here is not a stop. Failing the wait would drop every later event
                // of this poll and tell the caller the wait itself broke.
                if let Err(e) = self.client.attach_targets(&[&target_id]) {
                    self.runtime_warnings.push(format!(
                        "attach of started target {target_id} ({target_type}) refused: {e}"
                    ));
                    return Ok(None);
                }
                self.attached_targets.insert(target_id.clone(), target_type);
                Ok(None)
            }
            DebugEvent::TargetQuit { target_id } => {
                info!(target_id, "target quit");
                self.attached_targets.remove(&target_id);
                self.mark_resumed(&target_id);
                Ok(None)
            }
            DebugEvent::CallStackFormed {
                target_id,
                line_no,
                module_extension,
                module_object_id,
                module_property_id,
                send_message_only,
                call_stack,
                ..
            } => {
                if send_message_only {
                    if let Err(e) = self.client.step(&target_id, "Continue") {
                        self.runtime_warnings.push(format!(
                            "continue of a message-only stop of {target_id} refused: {e}"
                        ));
                    }
                    return Ok(None);
                }

                let depth = call_stack.len();
                let step = self.record_stop_event(&target_id, (depth > 0).then_some(depth));

                let module_id = ModuleId {
                    extension: module_extension,
                    object_id: module_object_id,
                    property_id: module_property_id,
                };
                let module_path = self
                    .index
                    .path_by_module(&module_id)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| module_id.to_string());

                // Neither 8.3.17 nor 8.3.27 puts a moduleID or a lineNo at the top level of this
                // event: both sit inside the innermost `callStack` frame, which is therefore the
                // only description of where execution stopped.
                let innermost = call_stack.first().map(|f| {
                    (
                        ModuleId {
                            extension: f.extension.clone(),
                            object_id: f.object_id.clone(),
                            property_id: f.property_id.clone(),
                        },
                        f.line_no,
                    )
                });

                // `stopByBP` stays true after a step too, so the session's own record tells the
                // two apart: the end of a step this session sent is a step even on a breakpoint
                // line, and any other stop is a breakpoint stop exactly where this session put
                // one. A stop deeper than the step could end — inside a call it stepped over —
                // is no step end. A breakpoint removed between the stop and this classification
                // reads as a step.
                let reason_at = if line_no > 0 && !module_id.object_id.is_empty() {
                    Some((module_id.clone(), line_no))
                } else {
                    innermost
                };
                let at_breakpoint = matches!(
                    &reason_at,
                    Some((mid, line)) if self.breakpoint_at(mid, *line)
                );
                let reason = match step {
                    Some(step) if !step.overshot(depth) => StopReason::Step,
                    _ if at_breakpoint => StopReason::Breakpoint,
                    _ => StopReason::Step,
                };

                let stack: Vec<FrameInfo> = call_stack
                    .into_iter()
                    .map(|f| {
                        let mid = ModuleId {
                            extension: f.extension,
                            object_id: f.object_id,
                            property_id: f.property_id,
                        };
                        let path = self
                            .index
                            .path_by_module(&mid)
                            .map(|p| p.display().to_string())
                            .unwrap_or_default();
                        FrameInfo {
                            module_path: path,
                            module_name: mid.to_string(),
                            line: f.line_no,
                            presentation: f.presentation,
                        }
                    })
                    .collect();

                // Index 0 is the current frame, so the innermost position is the first one.
                let (stop_module, stop_line) = if line_no > 0 && !module_path.is_empty() {
                    (module_path, line_no)
                } else if let Some(first) = stack.first() {
                    (first.module_path.clone(), first.line)
                } else {
                    (module_path, line_no)
                };

                Ok(Some(StopEvent {
                    reason,
                    target_id,
                    module: stop_module,
                    line: stop_line,
                    stack,
                    detail: None,
                }))
            }
            DebugEvent::RuntimeException {
                target_id,
                description,
                line_no,
                module_extension,
                module_object_id,
                module_property_id,
            } => {
                self.record_stop_event(&target_id, None);

                let module_id = ModuleId {
                    extension: module_extension,
                    object_id: module_object_id,
                    property_id: module_property_id,
                };
                let module_path = self
                    .index
                    .path_by_module(&module_id)
                    .map(|p| p.display().to_string())
                    .unwrap_or_else(|| module_id.to_string());

                let stack = self.call_stack().unwrap_or_default();

                Ok(Some(StopEvent {
                    reason: StopReason::Exception { message: description },
                    target_id,
                    module: module_path,
                    line: line_no,
                    stack,
                    detail: None,
                }))
            }
            DebugEvent::ExprEvaluated { result_id, raw_xml } => {
                if self.waiting_eval.as_deref() != Some(result_id.as_str()) {
                    return Ok(None);
                }
                debug!(result_id, "async expression evaluated, buffering");
                self.pending_eval_results.insert(result_id, raw_xml);
                Ok(None)
            }
        }
    }
}

fn in_debug_message() -> String {
    "the infobase is held by another debugger (the debug server answered `ibInDebug`): commands \
     from this one would be refused and no stop would ever reach it"
        .to_string()
}

/// The registration that holds the infobase may be one this user's own earlier session left
/// behind when it died; its id, re-registered, takes that place at once instead of after the
/// server's minute of patience, and keeps whatever that session left halted resumable.
fn reclaim_dead_session(
    lease_dir: Option<&Path>,
    lease_name: &str,
) -> Result<(Lease, String), ClientError> {
    let expiry = "a holder that died without detaching is dropped by the debug server after about \
                  a minute without polling; a target it left halted can stay frozen even then, \
                  and only restarting that client recovers it";
    let Some(dir) = lease_dir else {
        return Err(ClientError::AttachFailed(format!("{}; {expiry}", in_debug_message())));
    };
    match Lease::reclaim(dir, lease_name) {
        Ok(Reclaim::Reclaimed { lease, debugger_id }) => Ok((lease, debugger_id)),
        Ok(Reclaim::HeldByLiveSession) => Err(ClientError::AttachFailed(format!(
            "{}; the holder is another live debug session of this user (lease {}): disconnect \
             it first",
            in_debug_message(),
            dir.join(lease_name).display()
        ))),
        Ok(Reclaim::NoLease) => Err(ClientError::AttachFailed(format!(
            "{}; no session of this user left an id for this endpoint, so the holder is another \
             user, the Configurator, EDT, or a session of this user that reached the same server \
             under another host name; {expiry}",
            in_debug_message()
        ))),
        Err(e) => Err(ClientError::AttachFailed(format!(
            "{}; the lease of a previous session could not be read ({e}); {expiry}",
            in_debug_message()
        ))),
    }
}

#[cfg(test)]
mod tests {
    //! The session against a scripted debug server: answers are recorded traffic, the order and
    //! content of the requests the session sends are what is asserted.

    use super::*;
    use crate::types::responses::recorded;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::TcpListener;
    use std::sync::Mutex;

    const SERVER_TARGET: &str = "2a887b1e-1151-46c8-ab56-e0078edee53d";
    const CLIENT_TARGET: &str = "950b2b2e-bb8f-4a43-b9bf-1316114df630";

    /// Bound of a wait for an event that a regression would never deliver: 60 s under
    /// `timing_contract`, and ten minutes otherwise.
    const HANG_WATCHDOG: Duration =
        if cfg!(timing_contract) { Duration::from_secs(60) } else { Duration::from_secs(10 * 60) };

    const REGISTERED: &str = concat!(
        r#"<response xmlns:debugRDBGRequestResponse="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse">"#,
        "<debugRDBGRequestResponse:result>registered</debugRDBGRequestResponse:result></response>"
    );
    const IB_IN_DEBUG: &str = concat!(
        r#"<response xmlns:debugRDBGRequestResponse="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse">"#,
        "<debugRDBGRequestResponse:result>ibInDebug</debugRDBGRequestResponse:result></response>"
    );
    const NOT_REGISTERED: &str = concat!(
        r#"<exception xmlns="http://v8.1c.ru/8.2/virtual-resource-system" reason="400">"#,
        r#"<descr xmlns="http://v8.1c.ru/8.1/data/core">Ошибка при выполнении запроса POST к ресурсу /e1crdbg/rdbg:"#,
        "\nUI+ - часть отладки не зарегистрирована</descr></exception>"
    );

    fn targets_xml() -> String {
        format!(
            concat!(
                r#"<response xmlns:debugRDBGRequestResponse="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse">"#,
                "<debugRDBGRequestResponse:id><id>{}</id><infoBaseAlias>DefAlias</infoBaseAlias>",
                "<targetType>ManagedClient</targetType></debugRDBGRequestResponse:id>",
                "<debugRDBGRequestResponse:id><id>{}</id><infoBaseAlias>DefAlias</infoBaseAlias>",
                "<targetType>ServerEmulation</targetType></debugRDBGRequestResponse:id></response>"
            ),
            CLIENT_TARGET, SERVER_TARGET
        )
    }

    type Script = dyn Fn(&str, &str) -> (u16, String) + Send + Sync;

    /// A debug server that answers by script and records every request but the event polls.
    struct FakeServer {
        port: u16,
        requests: Arc<Mutex<Vec<(String, String)>>>,
    }

    impl FakeServer {
        fn start(script: Box<Script>) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let requests = Arc::new(Mutex::new(Vec::new()));
            let script: Arc<Script> = Arc::from(script);
            let log = requests.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(stream) = stream else { return };
                    let script = script.clone();
                    let log = log.clone();
                    std::thread::spawn(move || serve(stream, &*script, &log));
                }
            });
            Self { port, requests }
        }

        fn commands(&self) -> Vec<(String, String)> {
            self.requests.lock().unwrap().clone()
        }

        fn position(&self, cmd: &str, body_part: &str) -> Option<usize> {
            self.commands().iter().position(|(c, b)| c == cmd && b.contains(body_part))
        }
    }

    fn serve(stream: std::net::TcpStream, script: &Script, log: &Mutex<Vec<(String, String)>>) {
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut request_line = String::new();
        if reader.read_line(&mut request_line).is_err() {
            return;
        }
        let mut length = 0usize;
        loop {
            let mut header = String::new();
            if reader.read_line(&mut header).is_err() || header.trim().is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                if name.eq_ignore_ascii_case("content-length") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
        }
        let mut body = vec![0u8; length];
        let _ = reader.read_exact(&mut body);
        let body = String::from_utf8_lossy(&body).to_string();
        let cmd = request_line
            .split("cmd=")
            .nth(1)
            .map(|rest| rest.split(['&', ' ']).next().unwrap_or("").to_string())
            .unwrap_or_default();
        let (status, answer) = if cmd == "pingDebugUIParams" {
            std::thread::sleep(Duration::from_millis(20));
            script(&cmd, &body)
        } else {
            log.lock().unwrap().push((cmd.clone(), body.clone()));
            script(&cmd, &body)
        };
        let reason = if status < 300 { "OK" } else { "Bad Request" };
        let response = format!(
            "HTTP/1.1 {status} {reason}\r\nContent-Type: application/xml\r\nContent-Length: {}\r\n\
             Connection: close\r\n\r\n{answer}",
            answer.len()
        );
        let mut stream = stream;
        let _ = stream.write_all(response.as_bytes());
    }

    /// The common answers of a server whose server-side target is halted.
    fn halted_server(cmd: &str, _body: &str) -> (u16, String) {
        match cmd {
            "attachDebugUI" => (200, REGISTERED.into()),
            "getDbgTargets" => (200, targets_xml()),
            "getDbgAllTargetStates" => {
                (200, recorded::ALL_TARGET_STATES_WITH_A_HALTED_TARGET.into())
            }
            "getCallStack" => (200, recorded::GET_CALL_STACK.into()),
            "pingDebugUIParams" => (204, String::new()),
            _ => (200, String::new()),
        }
    }

    fn config_root() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let write = |rel: &str, text: &str| {
            let path = dir.path().join(rel);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write(
            "Configuration.xml",
            "<?xml version=\"1.0\"?><MetaDataObject><Configuration \
             uuid=\"00000000-0000-0000-0000-0000000000c0\"></Configuration></MetaDataObject>",
        );
        write(
            "CommonModules/ТестОтладки.xml",
            "<?xml version=\"1.0\"?><MetaDataObject><CommonModule \
             uuid=\"5b0c1d3e-7a01-4c11-9a01-000000000001\"></CommonModule></MetaDataObject>",
        );
        write("CommonModules/ТестОтладки/Ext/Module.bsl", "Функция Посчитать(Знач Н) Экспорт\n");
        dir
    }

    fn connect(
        server: &FakeServer,
        root: &Path,
        lease_dir: Option<&Path>,
    ) -> Result<DebugSession, ClientError> {
        DebugSession::connect(DebugConfig {
            host: "127.0.0.1".into(),
            port: server.port,
            infobase: "DefAlias".into(),
            config_root: root.to_path_buf(),
            extensions: Vec::new(),
            auto_attach: Vec::new(),
            lease_dir: lease_dir.map(Path::to_path_buf),
        })
    }

    #[test]
    fn a_target_halted_before_the_attach_is_reported_and_can_be_continued() {
        let server = FakeServer::start(Box::new(halted_server));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");

        assert_eq!(session.suspended_at_attach(), [SERVER_TARGET]);
        let states: Vec<(&str, &str)> =
            session.known_targets().iter().map(|t| (t.id.as_str(), t.state.as_str())).collect();
        assert!(states.contains(&(SERVER_TARGET, "StopOnNextLine")), "{states:?}");
        assert!(states.contains(&(CLIENT_TARGET, "Worked")), "{states:?}");

        let stop =
            session.wait_for_stop(Duration::from_secs(5)).unwrap().expect("the halt is reported");
        assert!(matches!(stop.reason, StopReason::Suspended), "{:?}", stop.reason);
        assert_eq!(stop.target_id, SERVER_TARGET);
        assert_eq!(stop.line, 4);
        assert!(stop.module.ends_with("Module.bsl"), "resolved to the source: {}", stop.module);
        assert!(stop.detail.is_none(), "{:?}", stop.detail);
        let stack_request = server.position("getCallStack", SERVER_TARGET).expect("stack asked");
        assert!(
            server.commands()[stack_request].1.contains("<id xmlns:dbg="),
            "getCallStack names the target in <id>: {:?}",
            server.commands()[stack_request]
        );

        session.continue_execution().expect("continue reaches the halted target");
        let step = server.position("step", SERVER_TARGET).expect("a step request for it");
        assert!(server.commands()[step].1.contains("<action>Continue</action>"));
        assert_eq!(session.stopped_count(), 0);
        assert!(
            session.wait_for_stop(Duration::from_millis(100)).unwrap().is_none(),
            "a resumed target is not reported again"
        );
    }

    #[test]
    fn a_halt_without_a_call_stack_is_still_reported_and_resumable() {
        let server = FakeServer::start(Box::new(|cmd: &str, body: &str| {
            if cmd == "getCallStack" {
                (204, String::new())
            } else {
                halted_server(cmd, body)
            }
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");

        let stop =
            session.wait_for_stop(Duration::from_secs(5)).unwrap().expect("the halt is reported");
        assert!(matches!(stop.reason, StopReason::Suspended));
        assert_eq!(stop.target_id, SERVER_TARGET);
        let detail = stop.detail.expect("an unknown position says why");
        assert!(detail.contains("continue work regardless"), "{detail}");
        session.continue_execution().expect("resumable without a position");
        assert!(server.position("step", SERVER_TARGET).is_some());
    }

    #[test]
    fn continue_names_the_halted_targets_when_asked_for_another() {
        let server = FakeServer::start(Box::new(halted_server));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");

        let err = session.continue_target(Some(CLIENT_TARGET)).unwrap_err();
        let text = err.to_string();
        assert!(text.contains(CLIENT_TARGET) && text.contains(SERVER_TARGET), "{text}");

        session.continue_target(Some(SERVER_TARGET)).expect("the halted one by id");
        assert!(
            session.wait_for_stop(Duration::from_millis(50)).unwrap().is_none(),
            "a target resumed before it was reported is not reported afterwards"
        );
    }

    fn stop_result(target: &str, target_type: &str) -> String {
        format!(
            concat!(
                r#"<debugRDBGRequestResponse:result xsi:type="debugRDBGRequestResponse:DBGUIExtCmdInfoCallStackFormed">"#,
                "<cmdID>callStackFormed</cmdID><targetID><id>{}</id><targetType>{}</targetType>",
                "</targetID><stopByBP>true</stopByBP></debugRDBGRequestResponse:result>"
            ),
            target, target_type
        )
    }

    /// Two stops arrive in one poll, so both sit in the event queue before the first wait: the
    /// input on which only the latest one used to be reported.
    #[test]
    fn two_stops_queued_between_waits_are_both_reported_latest_first() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = FakeServer::start(Box::new(move |cmd: &str, body: &str| match cmd {
            "getDbgAllTargetStates" => (200, String::new()),
            "pingDebugUIParams" if !flag.swap(true, std::sync::atomic::Ordering::SeqCst) => (
                200,
                format!(
                    concat!(
                        r#"<response xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" "#,
                        r#"xmlns:debugRDBGRequestResponse="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse">"#,
                        "{}{}</response>"
                    ),
                    stop_result(CLIENT_TARGET, "ManagedClient"),
                    stop_result(SERVER_TARGET, "ServerEmulation")
                ),
            ),
            _ => halted_server(cmd, body),
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");
        absorb_events_until(&mut session, "both stops were never delivered", |session| {
            session.stopped_count() == 2
        });

        let first = session.wait_for_stop(Duration::ZERO).unwrap().expect("a stop");
        assert_eq!(first.target_id, SERVER_TARGET, "the latest stop comes first");
        assert_eq!(session.stopped_count(), 2);

        session.continue_target(None).expect("continue the latest stop");
        assert!(server.position("step", SERVER_TARGET).is_some());
        assert_eq!(
            session.stopped_target(),
            Some(CLIENT_TARGET),
            "commands fall back to the target still halted"
        );

        let second = session.wait_for_stop(Duration::ZERO).unwrap().expect("the earlier stop");
        assert_eq!(second.target_id, CLIENT_TARGET, "the earlier stop is not dropped");
        session.continue_target(None).expect("continue the earlier stop");
        assert!(server.position("step", CLIENT_TARGET).is_some());
        assert_eq!(session.stopped_count(), 0);
    }

    fn ping_response(results: &str) -> String {
        format!(
            concat!(
                r#"<response xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" "#,
                r#"xmlns:debugRDBGRequestResponse="http://v8.1c.ru/8.3/debugger/debugRDBGRequestResponse">"#,
                "{}</response>"
            ),
            results
        )
    }

    /// A stop of the server target on line 4 of `ОбщийМодуль.ТестОтладки`, `stopByBP` set as
    /// the platform sets it after a step too.
    fn stop_on_line_4() -> String {
        stop_with_frames(&[4])
    }

    /// A stop of the server target with frames in `ОбщийМодуль.ТестОтладки` on `lines`, given
    /// outermost first as the server sends them; the last one is where execution stands.
    fn stop_with_frames(lines: &[u32]) -> String {
        let frames: String = lines
            .iter()
            .map(|line| {
                format!(
                    concat!(
                        "<callStack><moduleID>",
                        "<objectID>5b0c1d3e-7a01-4c11-9a01-000000000001</objectID>",
                        "<propertyID>{}</propertyID></moduleID><lineNo>{}</lineNo></callStack>"
                    ),
                    crate::constants::PROPERTY_MODULE,
                    line
                )
            })
            .collect();
        format!(
            concat!(
                r#"<debugRDBGRequestResponse:result xsi:type="debugRDBGRequestResponse:DBGUIExtCmdInfoCallStackFormed">"#,
                "<cmdID>callStackFormed</cmdID><targetID><id>{}</id>",
                "<targetType>ServerEmulation</targetType></targetID>{}",
                "<stopByBP>true</stopByBP></debugRDBGRequestResponse:result>"
            ),
            SERVER_TARGET, frames
        )
    }

    /// Handles the delivered events the way [`DebugSession::wait_for_stop`] does before it
    /// reports anything, stops queued unreported, until `done` holds. The events arrive from
    /// the listener thread one result at a time, so a single drain may see only some of them.
    fn absorb_events_until(
        session: &mut DebugSession,
        what: &str,
        mut done: impl FnMut(&mut DebugSession) -> bool,
    ) {
        let deadline = std::time::Instant::now() + HANG_WATCHDOG;
        loop {
            while let Ok(event) = session.events.try_recv() {
                if let Some(stop) = session.handle_event(event).unwrap() {
                    session.queue_stop(stop);
                }
            }
            if done(session) {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "{what}");
            std::thread::yield_now();
        }
    }

    /// The first stop that is not the announcement of a halt from before the attach. Until
    /// the server's own stop event is delivered, that announcement is all there is to report.
    fn wait_for_reported_stop(session: &mut DebugSession) -> StopEvent {
        loop {
            let stop = session.wait_for_stop(HANG_WATCHDOG).unwrap().expect("the reported stop");
            if !matches!(stop.reason, StopReason::Suspended) {
                return stop;
            }
        }
    }

    /// The target halted before the attach then reports a stop of its own: the input on which
    /// the leftover announcement used to report the same halt a second time.
    #[test]
    fn a_real_stop_replaces_the_halted_before_attach_announcement() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = FakeServer::start(Box::new(move |cmd: &str, body: &str| match cmd {
            "pingDebugUIParams" if !flag.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                (200, ping_response(&stop_on_line_4()))
            }
            _ => halted_server(cmd, body),
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");
        assert_eq!(session.suspended_at_attach(), [SERVER_TARGET]);

        // Until the server's stop is delivered the announcement is all there is to report, so
        // the gate waits for that delivery — the stop queued — and only then asks: the first
        // answer must be the stop, not the announcement it replaces.
        absorb_events_until(&mut session, "the server's stop is delivered", |session| {
            !session.unreported_stops.is_empty()
        });
        let stop = session.wait_for_stop(Duration::ZERO).unwrap().expect("the reported stop");
        assert!(!matches!(stop.reason, StopReason::Suspended), "{:?}", stop.reason);
        // A second report would come from the announcement the session already holds, so the
        // answer is there at once; the timing profile also leaves the event stream a window.
        let window = if cfg!(timing_contract) { Duration::from_millis(50) } else { Duration::ZERO };
        assert!(session.wait_for_stop(window).unwrap().is_none(), "one halt is reported once");
    }

    /// The step lands on the breakpoint line, and a later Continue reaches the same line: the
    /// first stop ends a step, the second is the breakpoint — the positive control that keeps
    /// the classification from passing by always answering `Step`.
    #[test]
    fn a_step_ending_on_a_breakpoint_line_is_a_step() {
        use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};
        let steps = Arc::new(AtomicUsize::new(0));
        let delivered = Arc::new(AtomicUsize::new(0));
        let (s, d) = (steps.clone(), delivered.clone());
        let server = FakeServer::start(Box::new(move |cmd: &str, body: &str| match cmd {
            "step" => {
                s.fetch_add(1, SeqCst);
                (200, String::new())
            }
            "pingDebugUIParams" if d.load(SeqCst) < s.load(SeqCst) => {
                d.fetch_add(1, SeqCst);
                (200, ping_response(&stop_on_line_4()))
            }
            _ => halted_server(cmd, body),
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");
        session.set_breakpoint("ОбщийМодуль.ТестОтладки", 4, None).unwrap();
        session.wait_for_stop(Duration::from_secs(5)).unwrap().expect("halted before the attach");

        session.step(StepAction::Next).unwrap();
        let stepped = session.wait_for_stop(Duration::from_secs(5)).unwrap().expect("step end");
        assert!(matches!(stepped.reason, StopReason::Step), "{:?}", stepped.reason);

        session.continue_execution().unwrap();
        let hit = session.wait_for_stop(Duration::from_secs(5)).unwrap().expect("breakpoint");
        assert!(matches!(hit.reason, StopReason::Breakpoint), "{:?}", hit.reason);
    }

    /// Both front ends attach by this list, and a file infobase reports only these two types.
    #[test]
    fn the_default_target_types_attach_a_file_infobase() {
        let server = FakeServer::start(Box::new(halted_server));
        let root = config_root();
        let session = DebugSession::connect(DebugConfig {
            host: "127.0.0.1".into(),
            port: server.port,
            infobase: "DefAlias".into(),
            config_root: root.path().to_path_buf(),
            extensions: Vec::new(),
            auto_attach: DEFAULT_AUTO_ATTACH.iter().map(|t| t.to_string()).collect(),
            lease_dir: None,
        })
        .expect("attach");

        assert_eq!(session.attached_count(), 2, "{:?}", session.setup_warnings());
        assert_eq!(session.suspended_at_attach(), [SERVER_TARGET]);
    }

    /// The step over line 1 runs into a call whose breakpoint on line 4 halts it one frame
    /// deeper than the step started: that halt is the breakpoint's, not the step's end.
    #[test]
    fn a_breakpoint_inside_a_stepped_over_call_is_a_breakpoint() {
        use std::sync::atomic::{AtomicBool, Ordering::SeqCst};
        let stepped = Arc::new(AtomicBool::new(false));
        let delivered = Arc::new(AtomicBool::new(false));
        let (s, d) = (stepped.clone(), delivered.clone());
        let server = FakeServer::start(Box::new(move |cmd: &str, body: &str| match cmd {
            "step" => {
                s.store(true, SeqCst);
                (200, String::new())
            }
            "pingDebugUIParams" if s.load(SeqCst) && !d.swap(true, SeqCst) => {
                (200, ping_response(&stop_with_frames(&[1, 4])))
            }
            _ => halted_server(cmd, body),
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");
        session.set_breakpoint("ОбщийМодуль.ТестОтладки", 4, None).unwrap();
        let start = session.wait_for_stop(Duration::from_secs(5)).unwrap().expect("halted");
        assert_eq!(start.stack.len(), 1, "the step starts one frame deep");

        session.step(StepAction::Next).unwrap();
        let hit = session.wait_for_stop(Duration::from_secs(5)).unwrap().expect("breakpoint");
        assert_eq!(hit.stack.len(), 2);
        assert!(matches!(hit.reason, StopReason::Breakpoint), "{:?}", hit.reason);
    }

    /// `getCallStack` that never answers in time: a caller that gives `wait_stop` no time still
    /// gets the known halt at once.
    #[test]
    fn a_zero_wait_reports_a_halt_before_the_attach_without_waiting_for_its_stack() {
        // The stack is answered only once the halt has been reported, so a wait that asked for
        // it never reports the halt before the watchdog.
        let (open_tx, open_rx) = std::sync::mpsc::channel::<()>();
        let stack_gate = Mutex::new(open_rx);
        let server = FakeServer::start(Box::new(move |cmd: &str, body: &str| {
            if cmd == "getCallStack" {
                let _ = stack_gate.lock().unwrap().recv_timeout(HANG_WATCHDOG);
            }
            halted_server(cmd, body)
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");

        let (answer_tx, answer) = std::sync::mpsc::channel();
        let waiter = std::thread::spawn(move || {
            let started = std::time::Instant::now();
            let stop = session.wait_for_stop(Duration::ZERO).unwrap();
            let _ = answer_tx.send((stop, started.elapsed()));
        });
        let answered = answer.recv_timeout(HANG_WATCHDOG);
        drop(open_tx);
        // Answered before the join: a thread still stuck past the watchdog is left behind
        // rather than waited on without a bound.
        let (stop, waited) = answered.expect("the halt was reported only after its stack");
        waiter.join().unwrap();
        let stop = stop.expect("the halt is reported");
        if cfg!(timing_contract) {
            assert!(waited < Duration::from_millis(500), "{waited:?}");
        }
        assert!(matches!(stop.reason, StopReason::Suspended));
        let detail = stop.detail.expect("an unknown position says why");
        assert!(detail.contains("not requested"), "{detail}");
    }

    #[test]
    fn a_halted_target_that_failed_to_attach_is_not_blamed_on_auto_attach() {
        let server = FakeServer::start(Box::new(|cmd: &str, body: &str| match cmd {
            "attachDetachDbgTargets" => (400, NOT_REGISTERED.into()),
            _ => halted_server(cmd, body),
        }));
        let root = config_root();
        let session = connect(&server, root.path(), None).expect("attach");

        assert!(session.suspended_at_attach().is_empty());
        let warnings = session.setup_warnings().join("\n");
        assert!(warnings.contains("attaching it failed"), "{warnings}");
        assert!(!warnings.contains("auto_attach leaves it out"), "{warnings}");
    }

    #[test]
    fn disconnect_clears_breakpoints_then_resumes_what_is_still_halted() {
        let server = FakeServer::start(Box::new(halted_server));
        let root = config_root();
        let leases = tempfile::tempdir().unwrap();
        let mut session = connect(&server, root.path(), Some(leases.path())).expect("attach");
        let lease = session.lease_path().expect("a lease is held").to_path_buf();
        session.set_breakpoint("ОбщийМодуль.ТестОтладки", 4, None).unwrap();

        let report = session.disconnect();
        assert_eq!(report.detach_error, None);
        assert_eq!(report.resumed, [SERVER_TARGET]);
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);

        let clear = server
            .commands()
            .iter()
            .rposition(|(c, b)| c == "setBreakpoints" && !b.contains("<bpInfo>"))
            .expect("an empty breakpoint list is sent");
        let resume = server.position("step", SERVER_TARGET).expect("the halted target resumed");
        let detach = server.position("detachDebugUI", "").expect("detached");
        assert!(
            clear < resume && resume < detach,
            "order: clear {clear}, resume {resume}, detach {detach}"
        );
        assert!(server.position("step", CLIENT_TARGET).is_none(), "a running target is left alone");
        let name = lease.file_name().unwrap().to_str().unwrap();
        assert!(
            matches!(Lease::reclaim(leases.path(), name).unwrap(), Reclaim::NoLease),
            "a clean detach leaves nothing to take over"
        );
    }

    #[test]
    fn an_infobase_held_by_a_dead_session_of_this_user_is_taken_over() {
        let leases = tempfile::tempdir().unwrap();
        let server = FakeServer::start(Box::new(|cmd: &str, body: &str| {
            if cmd == "attachDebugUI" {
                if body.contains("<idOfDebuggerUI>dead-session-id</idOfDebuggerUI>") {
                    (200, REGISTERED.into())
                } else {
                    (200, IB_IN_DEBUG.into())
                }
            } else {
                halted_server(cmd, body)
            }
        }));
        let name_for_port = Lease::file_name("127.0.0.1", server.port, "DefAlias");
        // What a killed process leaves: the file, without the lock.
        drop(Lease::acquire(leases.path(), &name_for_port, "dead-session-id").unwrap());

        let root = config_root();
        let mut session = connect(&server, root.path(), Some(leases.path())).expect("taken over");
        assert!(session.reclaimed_dead_session());
        let later = server.commands().into_iter().skip_while(|(c, _)| c == "attachDebugUI");
        for (cmd, body) in later {
            assert!(
                body.is_empty() || body.contains("dead-session-id"),
                "{cmd} must go out under the dead session's id: {body}"
            );
        }
        let stop = session.wait_for_stop(Duration::ZERO).unwrap().expect("its halt");
        assert_eq!(stop.target_id, SERVER_TARGET);
        session.continue_execution().unwrap();
    }

    #[test]
    fn an_infobase_held_by_someone_else_refuses_the_attach_instead_of_faking_one() {
        let server = FakeServer::start(Box::new(|cmd: &str, body: &str| {
            if cmd == "attachDebugUI" {
                (200, IB_IN_DEBUG.into())
            } else {
                halted_server(cmd, body)
            }
        }));
        let root = config_root();
        let leases = tempfile::tempdir().unwrap();
        let err = match connect(&server, root.path(), Some(leases.path())) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("ibInDebug must not produce a session"),
        };
        assert!(err.contains("ibInDebug"), "{err}");
        assert!(err.contains("no session of this user"), "{err}");
        assert!(server.position("attachDebugUI", "").is_some());
        assert!(
            server.position("getDbgTargets", "").is_none(),
            "nothing past the refusal: {:?}",
            server.commands()
        );
    }

    #[test]
    fn a_live_session_of_this_user_is_not_displaced() {
        let server = FakeServer::start(Box::new(|cmd: &str, body: &str| {
            if cmd == "attachDebugUI" {
                (200, IB_IN_DEBUG.into())
            } else {
                halted_server(cmd, body)
            }
        }));
        let leases = tempfile::tempdir().unwrap();
        let name = Lease::file_name("127.0.0.1", server.port, "DefAlias");
        let _live = Lease::acquire(leases.path(), &name, "live-id").unwrap().expect("lease");
        let root = config_root();
        let err = match connect(&server, root.path(), Some(leases.path())) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a live holder must not be displaced"),
        };
        assert!(err.contains("another live debug session"), "{err}");
        assert!(
            server.commands().iter().all(|(_, b)| !b.contains("live-id")),
            "the live session's id is never sent"
        );
    }

    #[test]
    fn a_refused_event_channel_is_reported() {
        let server = FakeServer::start(Box::new(|cmd: &str, body: &str| {
            if cmd == "pingDebugUIParams" {
                (400, NOT_REGISTERED.into())
            } else {
                halted_server(cmd, body)
            }
        }));
        let root = config_root();
        let session = connect(&server, root.path(), None).expect("attach");
        let deadline = std::time::Instant::now() + HANG_WATCHDOG;
        let error = loop {
            if let Some(e) = session.event_channel_error() {
                break e;
            }
            assert!(std::time::Instant::now() < deadline, "the refusal never surfaced");
            std::thread::sleep(Duration::from_millis(20));
        };
        assert!(error.contains("часть отладки не зарегистрирована"), "{error}");
    }

    /// A locals read whose asynchronous answer never arrives must not look like an empty frame.
    #[test]
    fn locals_without_an_answer_is_an_error_not_an_empty_frame() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = FakeServer::start(Box::new(move |cmd: &str, body: &str| match cmd {
            "pingDebugUIParams" if !flag.swap(true, std::sync::atomic::Ordering::SeqCst) => {
                (200, ping_response(&stop_on_line_4()))
            }
            "evalLocalVariables" => {
                assert!(body.contains("expressionResultID"), "{body}");
                (200, String::new())
            }
            _ => halted_server(cmd, body),
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");
        wait_for_reported_stop(&mut session);

        let err = session.locals(0).unwrap_err();
        assert!(
            err.to_string().contains("no result within 5 s"),
            "a missing locals answer must not read as an empty frame: {err}"
        );
    }

    #[test]
    fn a_message_only_stop_whose_continue_is_refused_does_not_fail_the_wait() {
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let server = FakeServer::start(Box::new(move |cmd: &str, body: &str| match cmd {
            "pingDebugUIParams" if !flag.swap(true, std::sync::atomic::Ordering::SeqCst) => (
                200,
                ping_response(&format!(
                    concat!(
                        r#"<debugRDBGRequestResponse:result xsi:type=\"debugRDBGRequestResponse:DBGUIExtCmdInfoCallStackFormed\">"#,
                        "<cmdID>callStackFormed</cmdID><targetID><id>{target}</id></targetID>",
                        "<sendMessageOnly>true</sendMessageOnly>",
                        "</debugRDBGRequestResponse:result>"
                    ),
                    target = SERVER_TARGET,
                )),
            ),
            "step" => (400, NOT_REGISTERED.into()),
            "getDbgAllTargetStates" => (200, String::new()),
            _ => halted_server(cmd, body),
        }));
        let root = config_root();
        let mut session = connect(&server, root.path(), None).expect("attach");
        let mut warnings = Vec::new();
        absorb_events_until(&mut session, "the refused continue was never reported", |session| {
            warnings.extend(session.take_runtime_warnings());
            warnings.iter().any(|w| w.contains("message-only") && w.contains("refused"))
        });

        // The message-only event has been handled above, so a stop made of it would be queued
        // already; the timing profile also leaves the event stream a window.
        let window =
            if cfg!(timing_contract) { Duration::from_millis(200) } else { Duration::ZERO };
        let stop = session.wait_for_stop(window).unwrap();
        assert!(stop.is_none(), "a message-only stop is not a stop: {stop:?}");
    }
}
