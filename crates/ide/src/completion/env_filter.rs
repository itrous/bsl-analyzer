//! Filters completion candidates by the execution environments of the code at
//! the cursor: a candidate the availability diagnostics would immediately
//! underline (`UnavailableInEnvironment`, `ModuleAccessibility`) is not
//! offered. The same model drives both sides — module environments ∩
//! compilation directive ∩ enclosing `#Если` regions on the caller, the
//! syntax-helper "Доступность" markup on the candidate, and the configured
//! `checked_environments` mask — so narrowing the code with `#Если НЕ
//! ВебКлиент` or unchecking the web client in `bsl-analyzer.toml` restores
//! the corresponding suggestions.

use hir::execution_env::{self, EnvFlags};
use ide_db::RootDatabase;
use syntax::TextSize;
use vfs::FileId;

pub(super) struct EnvFilter {
    body_env: EnvFlags,
    checked_env: EnvFlags,
}

impl EnvFilter {
    /// A filter that admits everything — for contexts where availability must
    /// not be judged (union receivers, tests).
    pub(super) fn permissive() -> Self {
        Self { body_env: EnvFlags::EMPTY, checked_env: EnvFlags::EMPTY }
    }

    pub(super) fn at<DB: RootDatabase>(db: &DB, file_id: FileId, offset: TextSize) -> Self {
        let opts = db.env_options();
        let body_env = hir::execution_environment_at(db, file_id, offset);
        Self { body_env, checked_env: opts.checked_environments }
    }

    /// Whether a member with availability `member_env` should be offered:
    /// exactly the complement of the diagnostic's verdict, so completion
    /// never suggests what the analyzer would underline. Unknown sides
    /// (empty masks) admit everything.
    pub(super) fn admits(&self, member_env: EnvFlags) -> bool {
        if self.body_env.is_empty() || member_env.is_empty() {
            return true;
        }
        (self.body_env.without(member_env) & self.checked_env).is_empty()
    }

    pub(super) fn admits_context(
        &self,
        context: Option<&bsl_platform::ContextAvailability>,
    ) -> bool {
        self.admits(EnvFlags::from_platform_context(context))
    }

    /// Whether a same-module method should be offered — the completion mirror
    /// of the local `ModuleAccessibility` rule: in a form or command module only
    /// the server side is a violation, a client-side caller reaching a server
    /// method is the form's regular remote call; an object-family module has no
    /// remote call.
    pub(super) fn admits_local_method(
        &self,
        callee_env: EnvFlags,
        module_type: bsl_metadata::ModuleType,
    ) -> bool {
        if self.body_env.is_empty() || callee_env.is_empty() {
            return true;
        }
        let mut missing = self.body_env.without(callee_env) & self.checked_env;
        if execution_env::local_server_call_is_remote(module_type) {
            missing = missing & EnvFlags::SERVER_SIDE;
        }
        missing.is_empty()
    }

    /// Whether the common module `name` is callable from the cursor's
    /// environments — the completion mirror of `ModuleAccessibility`:
    /// `ВызовСервера` modules stay visible to every client environment.
    /// Judged on the caller-visible flags; an extension that NARROWS an
    /// adopted module hides it here while the diagnostic tolerates that
    /// corner as a miss — the caller-visible model is what completion sells.
    pub(super) fn admits_common_module<DB: RootDatabase>(
        &self,
        db: &DB,
        file_id: FileId,
        name: &str,
    ) -> bool {
        if self.body_env.is_empty() {
            return true;
        }
        let Some(module) = db.resolve_common_module(file_id, name) else {
            return true;
        };
        let opts = db.env_options();
        let module_env = execution_env::common_module_env(&module, &opts);
        if module_env.is_empty() {
            return true;
        }
        let mut missing = self.body_env.without(module_env) & self.checked_env;
        if module.is_server_call() {
            missing = missing.without(EnvFlags::ALL_CLIENTS);
        }
        missing.is_empty()
    }
}
