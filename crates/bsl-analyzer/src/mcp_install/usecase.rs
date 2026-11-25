use crate::mcp_install::error::InstallError;
use crate::mcp_install::model::{
    InstallAction, InstallPlan, InstallRequest, InstallScope, InstallTarget, InstallTargetSelector,
};
use crate::mcp_install::preset;

pub(super) fn build_install_plan(mut request: InstallRequest) -> Result<InstallPlan, InstallError> {
    if request.name.trim().is_empty() {
        return Err(InstallError::EmptyName);
    }

    request.project_dir =
        request.project_dir.canonicalize().unwrap_or_else(|_| request.project_dir.clone());
    request.source_dir =
        request.source_dir.canonicalize().unwrap_or_else(|_| request.source_dir.clone());

    validate_preset_scope(&request)?;
    let targets = resolve_targets(request.target, request.scope)?;
    let spec = preset::build_server_spec(&request, &preset::resolve_self_binary());

    let actions = targets
        .into_iter()
        .map(|target| InstallAction {
            target,
            scope: request.scope,
            spec: spec.clone(),
            project_dir: request.project_dir.clone(),
            force: request.force,
            dry_run: request.dry_run,
        })
        .collect();

    Ok(InstallPlan { actions })
}

fn validate_preset_scope(request: &InstallRequest) -> Result<(), InstallError> {
    let supported = match request.preset {
        crate::mcp_install::model::InstallPreset::Workspace => InstallScope::Project,
        crate::mcp_install::model::InstallPreset::Reference => InstallScope::User,
    };

    if request.scope == supported {
        Ok(())
    } else {
        Err(InstallError::UnsupportedPresetScope { preset: request.preset, scope: request.scope })
    }
}

fn resolve_targets(
    selector: InstallTargetSelector,
    scope: InstallScope,
) -> Result<Vec<InstallTarget>, InstallError> {
    match selector {
        InstallTargetSelector::One(target) => {
            if !target_supports_scope(target, scope) {
                return Err(InstallError::UnsupportedScope { target, scope });
            }
            Ok(vec![target])
        }
        InstallTargetSelector::All => {
            if scope == InstallScope::Local {
                return Err(InstallError::UnsupportedAllScope { scope });
            }
            Ok(InstallTarget::ALL.to_vec())
        }
    }
}

fn target_supports_scope(target: InstallTarget, scope: InstallScope) -> bool {
    match target {
        InstallTarget::Codex => matches!(scope, InstallScope::User | InstallScope::Project),
        InstallTarget::Gemini => matches!(scope, InstallScope::User | InstallScope::Project),
        InstallTarget::Claude => true,
        InstallTarget::Cursor => matches!(scope, InstallScope::User | InstallScope::Project),
    }
}

#[cfg(test)]
mod tests {
    use std::{collections::BTreeMap, path::PathBuf};

    use crate::mcp_install::model::{
        normalize_source_dir_for_scope, InstallPreset, InstallRequest, InstallScope, InstallTarget,
        InstallTargetSelector,
    };

    use super::build_install_plan;

    fn request() -> InstallRequest {
        InstallRequest {
            target: InstallTargetSelector::One(InstallTarget::Cursor),
            scope: InstallScope::Project,
            preset: InstallPreset::Workspace,
            name: "bsl-analyzer".to_owned(),
            project_dir: PathBuf::from("/tmp/workspace"),
            source_dir: PathBuf::from("/tmp/workspace/src"),
            onec_url: None,
            onec_user: String::new(),
            onec_password: String::new(),
            env: BTreeMap::new(),
            enable_tools: Vec::new(),
            workspace_args: Vec::new(),
            force: false,
            dry_run: false,
        }
    }

    #[test]
    fn all_requires_shared_scope() {
        let mut req = request();
        req.target = InstallTargetSelector::All;
        req.scope = InstallScope::User;

        let err = build_install_plan(req).expect_err("workspace preset only supports project");
        assert!(err.to_string().contains("preset 'workspace' does not support 'user' scope"));
    }

    #[test]
    fn project_scope_uses_relative_source_dir() {
        let normalized = normalize_source_dir_for_scope(
            InstallScope::Project,
            PathBuf::from("/tmp/workspace").as_path(),
            PathBuf::from("/tmp/workspace/src").as_path(),
        );

        assert_eq!(normalized, "src");
    }

    #[test]
    fn user_scope_uses_absolute_source_dir() {
        let normalized = normalize_source_dir_for_scope(
            InstallScope::User,
            PathBuf::from("/tmp/workspace").as_path(),
            PathBuf::from("/tmp/workspace/src").as_path(),
        );

        assert_eq!(normalized, "/tmp/workspace/src");
    }

    #[test]
    fn reference_preset_requires_user_scope() {
        let mut req = request();
        req.preset = crate::mcp_install::model::InstallPreset::Reference;

        let err = build_install_plan(req).expect_err("reference preset only supports user scope");
        assert!(err.to_string().contains("preset 'reference' does not support 'project' scope"));
    }
}
