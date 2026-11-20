use std::sync::Arc;

#[salsa::input(singleton, debug)]
pub struct FeaturesInput {
    #[returns(copy)]
    pub type_narrowing: bool,
    #[returns(copy)]
    pub map_key_types: bool,
    #[returns(copy)]
    pub env_options: hir::execution_env::EnvOptions,
    /// `None` selects the attested bundled catalog release.
    #[returns(clone)]
    pub target_platform_version: Option<Arc<str>>,
    /// Oldest platform release the code must compile on; `None` disables
    /// `PlatformMemberNewerThanMinVersion`. Independent of the target above.
    #[returns(clone)]
    pub min_platform_version: Option<Arc<str>>,
    /// The compatibility mode the code compiles under, as written in the setting or
    /// `Configuration.xml` (`Version8_2_13`, `8.2.13`, `DontUse`); `None` when no
    /// source states one. Drives `PlatformMemberHiddenByCompatibilityMode`.
    #[returns(clone)]
    pub compatibility_mode: Option<Arc<str>>,
    /// The main configuration's vendor support map (`Ext/ParentConfigurations.bin`);
    /// `None` when not loaded: no diagnostic asked for it, or the configuration was
    /// never on support.
    #[returns(clone)]
    pub vendor_support: Option<Arc<bsl_metadata::VendorSupport>>,
}
