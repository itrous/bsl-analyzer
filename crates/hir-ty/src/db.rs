use bsl_types::kind::TypeId;
use hir_def::{ConfigsDatabase, DefWithBodyId, ExprId, MethodIdInput};
use std::sync::Arc;
use vfs::FileId;

use crate::infer::{
    BodyInferenceResult, InferenceDiagnostic, InferenceResult, ModuleCodeInferenceResult,
};
use crate::narrow::NarrowState;
use crate::proc_signature::ProcSignature;

#[salsa::db]
pub trait HirDatabase: ConfigsDatabase + bsl_types::intern::TypeKernelDb {
    fn infer(&self, file_id: FileId) -> Arc<InferenceResult>;

    fn type_of_expr(&self, file_id: FileId, owner: DefWithBodyId, expr: ExprId) -> TypeId;

    fn narrow(
        &self,
        file_id: FileId,
        owner: DefWithBodyId,
    ) -> Option<Arc<dataflow::DataflowResult<NarrowState>>>;

    /// Argument-shape diagnostics of one method; see `arg_diagnostics`.
    fn method_arg_diagnostics(&self, method: MethodIdInput<'_>) -> Arc<Vec<InferenceDiagnostic>>;

    fn module_code_arg_diagnostics(&self, file_id: FileId) -> Arc<Vec<InferenceDiagnostic>>;

    /// Every body's argument diagnostics of the file, paired with their
    /// owners: a fold over the per-body memos, not a memo of its own.
    fn arg_diagnostics(&self, file_id: FileId) -> Arc<Vec<(DefWithBodyId, InferenceDiagnostic)>>;

    fn type_narrowing_enabled(&self) -> bool;

    /// The `mapKeyTypes` profile: typed `Соответствие.Получить("key")` results.
    fn map_key_types_enabled(&self) -> bool;

    /// Project-level execution-environment settings for API-availability
    /// checks (which client environments the configuration targets).
    fn env_options(&self) -> hir_def::execution_env::EnvOptions;

    /// Configured 1C runtime target; `None` selects the bundled catalog release.
    fn target_platform_version(&self) -> Option<Arc<str>>;

    /// Oldest 1C platform release the project's code must compile on; `None`
    /// keeps the min-platform check silent. Never consulted for catalog selection.
    fn min_platform_version(&self) -> Option<Arc<str>>;

    /// The configuration's compatibility mode as written (`Version8_2_13`,
    /// `8.2.13`, `DontUse`); `None` when unknown. Below a per-name threshold the
    /// platform hides some globals whatever its release.
    fn compatibility_mode(&self) -> Option<Arc<str>>;

    /// Whether the host has finished the initial workspace/metadata load.
    fn workspace_load_complete(&self) -> bool;

    fn proc_signature(&self, method_input: MethodIdInput<'_>) -> Arc<ProcSignature>;

    fn infer_method(&self, method: MethodIdInput<'_>) -> Arc<BodyInferenceResult>;

    /// Borrowed variant of [`infer_method`](Self::infer_method) for read-only
    /// paths: no `Arc` refcount traffic per read.
    fn infer_method_ref<'db>(
        &'db self,
        method: MethodIdInput<'db>,
    ) -> &'db Arc<BodyInferenceResult>;

    fn infer_module_code(&self, file_id: FileId) -> Arc<ModuleCodeInferenceResult>;

    /// Borrowed variant of [`infer_module_code`](Self::infer_module_code); see
    /// [`infer_method_ref`](Self::infer_method_ref).
    fn infer_module_code_ref(&self, file_id: FileId) -> &Arc<ModuleCodeInferenceResult>;

    /// Reaching definitions of the module-level code, which is lowered from
    /// the file root and so has no method key.
    fn module_code_reaching_definitions(
        &self,
        file_id: FileId,
    ) -> Option<Arc<dataflow::reaching_defs::ReachingDefsResult>>;

    /// Reaching definitions of one method. A method-keyed query reads this
    /// rather than the module-wide table, whose value changes with every body
    /// in the file.
    fn method_reaching_definitions(
        &self,
        method: MethodIdInput<'_>,
    ) -> Option<Arc<dataflow::reaching_defs::ReachingDefsResult>>;
}
