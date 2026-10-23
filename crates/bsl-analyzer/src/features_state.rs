use crate::global_state::GlobalState;

/// Shared with the MCP resident and CLI `analyze`: one implementation, so every
/// host analyses under the same effective configuration.
pub use ide_host_core::{
    apply_compatibility_mode_to_db, apply_configuration_xml_settings_to_db, apply_features_to_db,
    apply_ordinary_app_support_to_db, apply_project_config_to_db, apply_project_settings_to_db,
    apply_project_to_db, apply_vendor_support_to_db,
};

impl GlobalState {
    pub fn update_features_config(&mut self) {
        let config =
            self.project.as_ref().map(|project| project.config.clone()).unwrap_or_default();
        apply_project_settings_to_db(
            self.analysis_host.raw_database_mut(),
            &config,
            self.project.as_ref(),
        );
        apply_vendor_support_to_db(self.analysis_host.raw_database_mut(), self.project.as_ref());
        if let Some(project) = self.project.as_ref() {
            if let Some(warning) =
                platform_help::source_change_warning(Some(&project.config), &project.root)
            {
                tracing::warn!("{warning}");
                self.show_warning_message(warning);
            }
        }
    }

    /// Reads the settings the main configuration's `Configuration.xml` states into the
    /// database ([`apply_configuration_xml_settings_to_db`]).
    pub(crate) fn apply_configuration_xml_settings(&mut self) {
        apply_configuration_xml_settings_to_db(
            self.analysis_host.raw_database_mut(),
            self.project.as_ref(),
        );
    }
}

#[cfg(test)]
mod tests {
    use crate::global_state::GlobalState;

    /// A shared session applies its project on every owner publication and every opened
    /// document, so an unchanged project must leave the database as it is: a write sends
    /// the inference that read the setting back to recompute. The project's
    /// `Configuration.xml` states the run-mode settings that applying the settings alone
    /// resets.
    #[test]
    fn re_applying_an_unchanged_project_keeps_the_database_revision() {
        let dir = tempfile::tempdir().unwrap();
        let cf = dir.path().join("src").join("cf");
        std::fs::create_dir_all(&cf).unwrap();
        std::fs::write(
            cf.join("Configuration.xml"),
            "<MetaDataObject><Configuration><Properties><DefaultRunMode>ManagedApplication\
             </DefaultRunMode><UseOrdinaryFormInManagedApplication>false\
             </UseOrdinaryFormInManagedApplication></Properties></Configuration>\
             </MetaDataObject>",
        )
        .unwrap();
        std::fs::write(dir.path().join("bsl-analyzer.toml"), "").unwrap();
        let (sender, _receiver) = crossbeam_channel::unbounded();
        let mut state = GlobalState::new(sender);
        state.project = Some(project_model::Project::new(dir.path()).unwrap());
        state.update_features_config();
        let applied = salsa::plumbing::current_revision(state.analysis_host.raw_database());

        state.update_features_config();

        assert_eq!(salsa::plumbing::current_revision(state.analysis_host.raw_database()), applied);
    }
}
