use crate::common_module::CommonModule;
use crate::defined_type::DefinedType;
use crate::error::Result;
use crate::event_subscription::EventSubscription;
use crate::http_service::HTTPService;
use crate::metadata_object::{MdoType, MetadataObject, Name};
use crate::register::Register;
use crate::role::Role;
use crate::scheduled_job::ScheduledJob;
use crate::traits::{MdObject, Module};
use crate::web_service::WebService;
use intern::NormName;
use rustc_hash::FxHashMap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use stdx::case::CaseExt;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Configuration {
    #[serde(rename = "uuid", default = "Uuid::new_v4")]
    uuid: Uuid,

    #[serde(rename = "name")]
    name: String,

    #[serde(rename = "commonModules", default)]
    common_modules: Vec<CommonModule>,

    #[serde(rename = "metadataObjects", default)]
    metadata_objects: Vec<MetadataObject>,

    #[serde(rename = "registers", default)]
    registers: Vec<Register>,

    #[serde(rename = "eventSubscriptions", default)]
    event_subscriptions: Vec<EventSubscription>,

    #[serde(rename = "definedTypes", default)]
    defined_types: Vec<DefinedType>,

    #[serde(rename = "scheduledJobs", default)]
    scheduled_jobs: Vec<ScheduledJob>,

    #[serde(rename = "roles", default)]
    roles: Vec<Role>,

    #[serde(rename = "subsystems", default)]
    subsystems: Vec<crate::subsystem::Subsystem>,

    #[serde(rename = "httpServices", default)]
    http_services: Vec<HTTPService>,

    #[serde(rename = "webServices", default)]
    web_services: Vec<WebService>,

    #[serde(rename = "integrationServices", default)]
    integration_services: Vec<crate::integration_service::IntegrationService>,

    #[serde(skip)]
    uri_to_module: HashMap<String, usize>,

    /// Lowercased common-module URI -> index, so a case-insensitive URI lookup is
    /// O(1) instead of an O(all-modules) scan that re-lowercased every module's
    /// (Cyrillic) joined path on each file resolution.
    #[serde(skip)]
    uri_lower_to_common_module: HashMap<String, usize>,

    #[serde(skip)]
    name_to_common_module: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_register: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_event_subscription: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_defined_type: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_scheduled_job: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_role: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_http_service: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_web_service: FxHashMap<NormName, usize>,

    #[serde(skip)]
    name_to_integration_service: FxHashMap<NormName, usize>,

    /// `(mdo_type, lowercased name) -> index`, so [`Configuration::find_metadata_object`]
    /// is O(1) instead of an O(all-objects) scan that lowercased every object's
    /// (Cyrillic) name on each lookup.
    #[serde(skip)]
    metadata_objects_by_key: FxHashMap<(MdoType, NormName), usize>,

    #[serde(skip)]
    recorders_by_register: HashMap<(MdoType, Name), Vec<Name>>,

    #[serde(rename = "useManagedFormInOrdinaryApplication", default)]
    use_managed_form_in_ordinary_application: bool,

    #[serde(rename = "useOrdinaryFormInManagedApplication", default)]
    use_ordinary_form_in_managed_application: bool,
}

impl PartialEq for Configuration {
    fn eq(&self, other: &Self) -> bool {
        self.uuid == other.uuid
            && self.name == other.name
            && self.common_modules == other.common_modules
            && self.metadata_objects == other.metadata_objects
            && self.registers == other.registers
            && self.event_subscriptions == other.event_subscriptions
            && self.defined_types == other.defined_types
            && self.scheduled_jobs == other.scheduled_jobs
            && self.roles == other.roles
            && self.subsystems == other.subsystems
            && self.http_services == other.http_services
            && self.web_services == other.web_services
            && self.integration_services == other.integration_services
            && self.use_managed_form_in_ordinary_application
                == other.use_managed_form_in_ordinary_application
            && self.use_ordinary_form_in_managed_application
                == other.use_ordinary_form_in_managed_application
    }
}

impl Configuration {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            uuid: Uuid::new_v4(),
            name: name.into(),
            common_modules: Vec::new(),
            metadata_objects: Vec::new(),
            registers: Vec::new(),
            event_subscriptions: Vec::new(),
            defined_types: Vec::new(),
            scheduled_jobs: Vec::new(),
            roles: Vec::new(),
            subsystems: Vec::new(),
            uri_to_module: HashMap::new(),
            uri_lower_to_common_module: HashMap::new(),
            name_to_common_module: FxHashMap::default(),
            name_to_register: FxHashMap::default(),
            name_to_event_subscription: FxHashMap::default(),
            name_to_defined_type: FxHashMap::default(),
            name_to_scheduled_job: FxHashMap::default(),
            name_to_role: FxHashMap::default(),
            name_to_http_service: FxHashMap::default(),
            name_to_web_service: FxHashMap::default(),
            name_to_integration_service: FxHashMap::default(),
            metadata_objects_by_key: FxHashMap::default(),
            recorders_by_register: HashMap::new(),
            use_managed_form_in_ordinary_application: false,
            use_ordinary_form_in_managed_application: false,
            http_services: Vec::new(),
            web_services: Vec::new(),
            integration_services: Vec::new(),
        }
    }

    pub fn from_xml_str(xml: &str) -> Result<Self> {
        let doc = roxmltree::Document::parse(xml).map_err(|e| {
            crate::error::MetadataError::InvalidFormat(format!("XML parse error: {}", e))
        })?;

        let root = doc.root_element();
        let config_node = root.children().find(|n| n.is_element()).ok_or_else(|| {
            crate::error::MetadataError::InvalidFormat("No Configuration element".to_string())
        })?;

        let uuid_str = config_node.attribute("uuid").unwrap_or("");
        let uuid = uuid_str.parse::<uuid::Uuid>().unwrap_or_else(|_| uuid::Uuid::new_v4());

        let (
            name,
            use_managed_form_in_ordinary_application,
            use_ordinary_form_in_managed_application,
        ) = if let Some(props) =
            config_node.children().find(|n| n.is_element() && n.tag_name().name() == "Properties")
        {
            let name = props
                .children()
                .find(|n| n.is_element() && n.tag_name().name() == "Name")
                .and_then(|n| n.text())
                .unwrap_or("")
                .to_string();
            let managed = props
                .children()
                .find(|n| {
                    n.is_element() && n.tag_name().name() == "UseManagedFormInOrdinaryApplication"
                })
                .and_then(|n| n.text())
                .is_some_and(|s| s.eq_ignore_ascii_case("true"));
            let ordinary = props
                .children()
                .find(|n| {
                    n.is_element() && n.tag_name().name() == "UseOrdinaryFormInManagedApplication"
                })
                .and_then(|n| n.text())
                .is_some_and(|s| s.eq_ignore_ascii_case("true"));
            (name, managed, ordinary)
        } else {
            (String::new(), false, false)
        };

        let mut config = Configuration::new(name);
        config.uuid = uuid;
        config.use_managed_form_in_ordinary_application = use_managed_form_in_ordinary_application;
        config.use_ordinary_form_in_managed_application = use_ordinary_form_in_managed_application;
        Ok(config)
    }

    #[allow(dead_code)]
    fn build_caches(&mut self) {
        // Preserve the effective same-name MDO choices made by overlay merging.
        // Raw configurations have first-wins entries here; an overlay may have
        // intentionally redirected a key to an independent extension object.
        let previous_metadata_object_index = std::mem::take(&mut self.metadata_objects_by_key);
        self.uri_to_module.clear();
        self.uri_lower_to_common_module.clear();
        self.name_to_common_module.clear();
        self.name_to_register.clear();
        self.name_to_event_subscription.clear();
        self.name_to_defined_type.clear();
        self.name_to_scheduled_job.clear();
        self.name_to_role.clear();
        self.name_to_http_service.clear();
        self.name_to_web_service.clear();
        self.name_to_integration_service.clear();
        self.recorders_by_register.clear();

        for (idx, object) in self.metadata_objects.iter().enumerate() {
            // First occurrence wins, matching the replaced `.iter().find()` scan.
            self.metadata_objects_by_key
                .entry((object.mdo_type, NormName::intern(&object.name)))
                .or_insert(idx);
        }
        for (key, idx) in previous_metadata_object_index {
            if self
                .metadata_objects
                .get(idx)
                .is_some_and(|object| (object.mdo_type, NormName::intern(&object.name)) == key)
            {
                self.metadata_objects_by_key.insert(key, idx);
            }
        }

        for (idx, module) in self.common_modules.iter().enumerate() {
            if let Some(uri) = module.uri() {
                self.uri_to_module.insert(uri.to_string(), idx);
                // First occurrence wins, matching the replaced `.iter().find()` scan.
                self.uri_lower_to_common_module.entry(uri.fold_lower()).or_insert(idx);
            }
            self.name_to_common_module.insert(NormName::intern(module.name()), idx);
        }

        for (idx, register) in self.registers.iter().enumerate() {
            self.name_to_register.insert(NormName::intern(register.name()), idx);
        }

        for (idx, event_sub) in self.event_subscriptions.iter().enumerate() {
            self.name_to_event_subscription.insert(NormName::intern(event_sub.name()), idx);
        }

        for (idx, defined_type) in self.defined_types.iter().enumerate() {
            self.name_to_defined_type.insert(NormName::intern(defined_type.name()), idx);
        }

        for (idx, scheduled_job) in self.scheduled_jobs.iter().enumerate() {
            self.name_to_scheduled_job.insert(NormName::intern(scheduled_job.name()), idx);
        }

        for (idx, role) in self.roles.iter().enumerate() {
            self.name_to_role.insert(NormName::intern(role.name()), idx);
        }

        for (idx, http_service) in self.http_services.iter().enumerate() {
            self.name_to_http_service.insert(NormName::intern(http_service.name()), idx);
        }

        for (idx, web_service) in self.web_services.iter().enumerate() {
            self.name_to_web_service.insert(NormName::intern(web_service.name()), idx);
        }

        for (idx, integration_service) in self.integration_services.iter().enumerate() {
            self.name_to_integration_service
                .insert(NormName::intern(integration_service.name()), idx);
        }

        for object in &self.metadata_objects {
            index_document_recorders(&mut self.recorders_by_register, object);
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn uuid(&self) -> &Uuid {
        &self.uuid
    }

    pub fn common_modules(&self) -> &[CommonModule] {
        &self.common_modules
    }

    pub fn find_common_module(&self, name: &str) -> Option<&CommonModule> {
        self.name_to_common_module
            .get(&NormName::intern(name))
            .and_then(|&idx| self.common_modules.get(idx))
    }

    /// Case-insensitive lookup by root-relative URI. `uri_lower` must already be
    /// lowercased by the caller (the relative path of the module body file).
    pub fn find_common_module_by_uri_lower(&self, uri_lower: &str) -> Option<&CommonModule> {
        self.uri_lower_to_common_module.get(uri_lower).and_then(|&idx| self.common_modules.get(idx))
    }

    pub fn find_module_by_uri(&self, uri: &str) -> Option<&dyn Module> {
        self.uri_to_module
            .get(uri)
            .and_then(|&idx| self.common_modules.get(idx))
            .map(|cm| cm as &dyn Module)
    }

    pub fn find_child_by_uri(&self, uri: &str) -> Option<&dyn MdObject> {
        self.uri_to_module
            .get(uri)
            .and_then(|&idx| self.common_modules.get(idx))
            .map(|cm| cm as &dyn MdObject)
    }

    pub fn add_common_module(&mut self, module: CommonModule) {
        let idx = self.common_modules.len();

        if let Some(uri) = module.uri() {
            self.uri_to_module.insert(uri.to_string(), idx);
            // First occurrence wins, matching the replaced `.iter().find()` scan.
            self.uri_lower_to_common_module.entry(uri.fold_lower()).or_insert(idx);
        }
        self.name_to_common_module.insert(NormName::intern(module.name()), idx);

        self.common_modules.push(module);
    }

    pub fn use_managed_form_in_ordinary_application(&self) -> bool {
        self.use_managed_form_in_ordinary_application
    }

    pub fn use_ordinary_form_in_managed_application(&self) -> bool {
        self.use_ordinary_form_in_managed_application
    }

    pub fn metadata_objects(&self) -> &[MetadataObject] {
        &self.metadata_objects
    }

    pub fn add_metadata_object(&mut self, object: MetadataObject) {
        index_document_recorders(&mut self.recorders_by_register, &object);
        let idx = self.metadata_objects.len();
        // First occurrence wins, matching the replaced `.iter().find()` scan: a
        // later same-(type,name) object (e.g. a case-only-differing extension
        // overlay) must not shadow the base object the scan would have returned.
        self.metadata_objects_by_key
            .entry((object.mdo_type, NormName::intern(&object.name)))
            .or_insert(idx);
        self.metadata_objects.push(object);
    }

    pub fn merge_extension_overlay(&mut self, extension: &Configuration) {
        for ext_module in &extension.common_modules {
            if let Some(base_module) =
                self.common_modules.iter_mut().find(|module| ext_module.adopts(module))
            {
                base_module.apply_extension_overlay(ext_module);
                continue;
            }
            self.add_common_module(ext_module.clone());
        }

        for ext_obj in &extension.metadata_objects {
            if let Some(base_obj) = self.metadata_objects.iter_mut().find(|obj| ext_obj.adopts(obj))
            {
                base_obj.apply_extension_overlay(ext_obj);
                continue;
            }
            let idx = self.metadata_objects.len();
            self.add_metadata_object(ext_obj.clone());
            self.metadata_objects_by_key
                .insert((ext_obj.mdo_type, NormName::intern(&ext_obj.name)), idx);
        }

        for ext_reg in &extension.registers {
            if let Some(idx) = self.registers.iter().position(|base| {
                base.mdo_type() == ext_reg.mdo_type()
                    && stdx::case::eq_ignore_case(base.name(), ext_reg.name())
            }) {
                // An extension can add measurements/resources/attributes to a
                // borrowed register, so merge rather than ignore the adopted copy.
                self.registers[idx].apply_extension_overlay(ext_reg);
            } else {
                self.add_register(ext_reg.clone());
            }
        }

        for ext_defined_type in &extension.defined_types {
            if let Some(idx) = self
                .defined_types
                .iter()
                .position(|base| stdx::case::eq_ignore_case(base.name(), ext_defined_type.name()))
            {
                // An extension can refine a borrowed defined type's composition, so
                // take the extension's underlying type rather than ignore it.
                self.defined_types[idx].apply_extension_overlay(ext_defined_type);
            } else {
                self.add_defined_type(ext_defined_type.clone());
            }
        }

        for ext_subsystem in &extension.subsystems {
            if let Some(base) = self
                .subsystems
                .iter_mut()
                .find(|s| s.name().fold_lower() == ext_subsystem.name().fold_lower())
            {
                // An extension can add objects/child subsystems to an existing subsystem.
                base.merge_from(ext_subsystem);
            } else {
                self.add_subsystem(ext_subsystem.clone());
            }
        }

        self.build_caches();
    }

    pub fn merged_with_extension(&self, extension: &Configuration) -> Self {
        let mut merged = self.clone();
        merged.merge_extension_overlay(extension);
        merged
    }

    /// Attach to every object and register the common attributes whose composition includes
    /// it. Runs once over a freshly loaded root, before any extension overlay is folded in.
    pub fn apply_common_attributes(&mut self, set: &crate::common_attribute::CommonAttributeSet) {
        if set.is_empty() {
            return;
        }
        for object in &mut self.metadata_objects {
            let applied = set.for_object(object.mdo_type, &object.name);
            if !applied.is_empty() {
                object.set_common_attributes(applied);
            }
        }
        for register in &mut self.registers {
            let applied = set.for_object(register.mdo_type(), register.name());
            if !applied.is_empty() {
                register.set_common_attributes(applied);
            }
        }
    }

    pub fn has_metadata_object(&self, mdo_type: MdoType, name: &str) -> bool {
        let name_lower = name.fold_lower();

        let result = match mdo_type {
            MdoType::InformationRegister
            | MdoType::AccumulationRegister
            | MdoType::AccountingRegister
            | MdoType::CalculationRegister => self
                .registers
                .iter()
                .any(|reg| reg.mdo_type() == mdo_type && reg.name().fold_lower() == name_lower),
            _ => self
                .metadata_objects
                .iter()
                .any(|obj| obj.mdo_type == mdo_type && obj.name.fold_lower() == name_lower),
        };

        result
    }

    pub fn find_metadata_object(&self, mdo_type: MdoType, name: &str) -> Option<&MetadataObject> {
        let idx = *self.metadata_objects_by_key.get(&(mdo_type, NormName::intern(name)))?;
        self.metadata_objects.get(idx)
    }

    pub fn registers(&self) -> &[Register] {
        &self.registers
    }

    pub fn find_register(&self, name: &str) -> Option<&Register> {
        self.name_to_register.get(&NormName::intern(name)).and_then(|&idx| self.registers.get(idx))
    }

    pub fn find_register_by_type_and_name(
        &self,
        mdo_type: MdoType,
        name: &str,
    ) -> Option<&Register> {
        self.name_to_register
            .get(&NormName::intern(name))
            .and_then(|&idx| self.registers.get(idx).filter(|r| r.mdo_type() == mdo_type))
    }

    pub fn recorders_for_register(&self, parent: MdoType, name: &str) -> &[Name] {
        let key = (parent, name.fold_lower());
        self.recorders_by_register.get(&key).map(Vec::as_slice).unwrap_or(&[])
    }

    pub fn add_register(&mut self, register: Register) {
        let idx = self.registers.len();
        self.name_to_register.insert(NormName::intern(register.name()), idx);
        self.registers.push(register);
    }

    pub fn event_subscriptions(&self) -> &[EventSubscription] {
        &self.event_subscriptions
    }

    pub fn find_event_subscription(&self, name: &str) -> Option<&EventSubscription> {
        self.name_to_event_subscription
            .get(&NormName::intern(name))
            .and_then(|&idx| self.event_subscriptions.get(idx))
    }

    pub(crate) fn add_event_subscription(&mut self, subscription: EventSubscription) {
        let idx = self.event_subscriptions.len();
        self.name_to_event_subscription.insert(NormName::intern(subscription.name()), idx);
        self.event_subscriptions.push(subscription);
    }

    pub fn subsystems(&self) -> &[crate::subsystem::Subsystem] {
        &self.subsystems
    }

    pub(crate) fn add_subsystem(&mut self, subsystem: crate::subsystem::Subsystem) {
        self.subsystems.push(subsystem);
    }

    pub fn defined_types(&self) -> &[DefinedType] {
        &self.defined_types
    }

    pub fn find_defined_type(&self, name: &str) -> Option<&DefinedType> {
        self.name_to_defined_type
            .get(&NormName::intern(name))
            .and_then(|&idx| self.defined_types.get(idx))
    }

    pub fn add_defined_type(&mut self, defined_type: DefinedType) {
        let idx = self.defined_types.len();
        self.name_to_defined_type.insert(NormName::intern(defined_type.name()), idx);
        self.defined_types.push(defined_type);
    }

    pub fn scheduled_jobs(&self) -> &[ScheduledJob] {
        &self.scheduled_jobs
    }

    pub fn find_scheduled_job(&self, name: &str) -> Option<&ScheduledJob> {
        self.name_to_scheduled_job
            .get(&NormName::intern(name))
            .and_then(|&idx| self.scheduled_jobs.get(idx))
    }

    pub(crate) fn add_scheduled_job(&mut self, job: ScheduledJob) {
        let idx = self.scheduled_jobs.len();
        self.name_to_scheduled_job.insert(NormName::intern(job.name()), idx);
        self.scheduled_jobs.push(job);
    }

    pub fn roles(&self) -> &[Role] {
        &self.roles
    }

    pub fn find_role(&self, name: &str) -> Option<&Role> {
        self.name_to_role.get(&NormName::intern(name)).and_then(|&idx| self.roles.get(idx))
    }

    pub fn add_role(&mut self, role: Role) {
        let idx = self.roles.len();
        self.name_to_role.insert(NormName::intern(role.name()), idx);
        self.roles.push(role);
    }

    pub fn http_services(&self) -> &[HTTPService] {
        &self.http_services
    }

    pub fn find_http_service(&self, name: &str) -> Option<&HTTPService> {
        self.name_to_http_service
            .get(&NormName::intern(name))
            .and_then(|&idx| self.http_services.get(idx))
    }

    pub(crate) fn add_http_service(&mut self, http_service: HTTPService) {
        let idx = self.http_services.len();
        self.name_to_http_service.insert(NormName::intern(http_service.name()), idx);
        self.http_services.push(http_service);
    }

    pub fn web_services(&self) -> &[WebService] {
        &self.web_services
    }

    pub fn find_web_service(&self, name: &str) -> Option<&WebService> {
        self.name_to_web_service
            .get(&NormName::intern(name))
            .and_then(|&idx| self.web_services.get(idx))
    }

    pub(crate) fn add_web_service(&mut self, web_service: WebService) {
        let idx = self.web_services.len();
        self.name_to_web_service.insert(NormName::intern(web_service.name()), idx);
        self.web_services.push(web_service);
    }

    pub fn integration_services(&self) -> &[crate::integration_service::IntegrationService] {
        &self.integration_services
    }

    pub fn find_integration_service(
        &self,
        name: &str,
    ) -> Option<&crate::integration_service::IntegrationService> {
        self.name_to_integration_service
            .get(&NormName::intern(name))
            .and_then(|&idx| self.integration_services.get(idx))
    }

    pub(crate) fn add_integration_service(
        &mut self,
        integration_service: crate::integration_service::IntegrationService,
    ) {
        let idx = self.integration_services.len();
        self.name_to_integration_service.insert(NormName::intern(integration_service.name()), idx);
        self.integration_services.push(integration_service);
    }

    /// Heap bytes owned by this configuration, memoised by `ide-db`'s
    /// `load_configuration`/`merged_configuration` for Salsa's `heap_size` hook:
    /// its name, every owned metadata-family vec (recursing into each entry's own
    /// owned payload), and every `name -> index` lookup table `build_caches`
    /// maintains alongside them. New heap-owning fields must be added here too.
    pub fn estimated_heap_size(&self) -> usize {
        let mut bytes = self.name.capacity();

        bytes += stdx::heap::vec_bytes::<CommonModule>(self.common_modules.len())
            + self.common_modules.iter().map(CommonModule::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<MetadataObject>(self.metadata_objects.len())
            + self.metadata_objects.iter().map(MetadataObject::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<Register>(self.registers.len())
            + self.registers.iter().map(Register::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<EventSubscription>(self.event_subscriptions.len())
            + self
                .event_subscriptions
                .iter()
                .map(EventSubscription::estimated_heap_size)
                .sum::<usize>();
        bytes += stdx::heap::vec_bytes::<DefinedType>(self.defined_types.len())
            + self.defined_types.iter().map(DefinedType::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<ScheduledJob>(self.scheduled_jobs.len())
            + self.scheduled_jobs.iter().map(ScheduledJob::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<Role>(self.roles.len())
            + self.roles.iter().map(Role::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<crate::subsystem::Subsystem>(self.subsystems.len())
            + self
                .subsystems
                .iter()
                .map(crate::subsystem::Subsystem::estimated_heap_size)
                .sum::<usize>();
        bytes += stdx::heap::vec_bytes::<HTTPService>(self.http_services.len())
            + self.http_services.iter().map(HTTPService::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<WebService>(self.web_services.len())
            + self.web_services.iter().map(WebService::estimated_heap_size).sum::<usize>();
        bytes += stdx::heap::vec_bytes::<crate::integration_service::IntegrationService>(
            self.integration_services.len(),
        ) + self
            .integration_services
            .iter()
            .map(crate::integration_service::IntegrationService::estimated_heap_size)
            .sum::<usize>();

        bytes += name_index_heap(&self.uri_to_module);
        bytes += name_index_heap(&self.uri_lower_to_common_module);
        bytes += norm_index_heap(&self.name_to_common_module);
        bytes += norm_index_heap(&self.name_to_register);
        bytes += norm_index_heap(&self.name_to_event_subscription);
        bytes += norm_index_heap(&self.name_to_defined_type);
        bytes += norm_index_heap(&self.name_to_scheduled_job);
        bytes += norm_index_heap(&self.name_to_role);
        bytes += norm_index_heap(&self.name_to_http_service);
        bytes += norm_index_heap(&self.name_to_web_service);
        bytes += norm_index_heap(&self.name_to_integration_service);

        bytes += stdx::heap::map_table_bytes::<(MdoType, NormName), usize>(
            self.metadata_objects_by_key.len(),
        );

        bytes += stdx::heap::map_table_bytes::<(MdoType, Name), Vec<Name>>(
            self.recorders_by_register.len(),
        ) + self
            .recorders_by_register
            .iter()
            .map(|((_, key_name), recorders)| {
                key_name.capacity()
                    + stdx::heap::vec_bytes::<Name>(recorders.len())
                    + recorders.iter().map(String::capacity).sum::<usize>()
            })
            .sum::<usize>();

        bytes
    }
}

/// Heap of a `name -> index` lookup table (every `Configuration` cache follows this
/// shape): the table itself plus the owned lowercased-name keys.
/// Heap of a `NormName -> index` lookup table: just the table itself — the
/// interned key strings are owned by the global pool and counted once there.
fn norm_index_heap(map: &FxHashMap<NormName, usize>) -> usize {
    stdx::heap::map_table_bytes::<NormName, usize>(map.len())
}

fn name_index_heap(map: &HashMap<String, usize>) -> usize {
    stdx::heap::map_table_bytes::<String, usize>(map.len())
        + map.keys().map(String::capacity).sum::<usize>()
}

fn index_document_recorders(
    recorders_by_register: &mut HashMap<(MdoType, Name), Vec<Name>>,
    object: &MetadataObject,
) {
    if object.mdo_type != MdoType::Document {
        return;
    }

    for (register_type, register_name) in object.register_records() {
        let key = (*register_type, register_name.fold_lower());
        let documents = recorders_by_register.entry(key).or_default();
        if !documents.iter().any(|name| stdx::case::eq_ignore_case(name, &object.name)) {
            documents.push(object.name.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::enums::ReturnValueReuse;
    use crate::metadata_object::AttributeType;

    #[test]
    fn test_configuration_creation() {
        let config = Configuration::new("TestConfiguration");
        assert_eq!(config.name(), "TestConfiguration");
        assert_eq!(config.common_modules().len(), 0);
    }

    #[test]
    fn test_add_and_find_common_module() {
        let mut config = Configuration::new("Test");

        let module = CommonModule::builder()
            .name("TestModule")
            .uri(Some("CommonModules/TestModule/Ext/Module.bsl"))
            .return_values_reuse(ReturnValueReuse::DuringRequest)
            .build();

        config.add_common_module(module);

        assert_eq!(config.common_modules().len(), 1);

        let found = config.find_common_module("TestModule");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name(), "TestModule");

        let found_ci = config.find_common_module("testmodule");
        assert!(found_ci.is_some());

        let found_uri = config.find_module_by_uri("CommonModules/TestModule/Ext/Module.bsl");
        assert!(found_uri.is_some());
        assert_eq!(found_uri.unwrap().name(), "TestModule");
    }

    #[test]
    fn find_common_module_matches_every_case_variant_including_yo() {
        // The name index is keyed by interned NormName, whose identity is
        // eq_ignore_case — every spelling of the same name must hit, and Ё/Е
        // must stay distinct (they are different letters, not case variants).
        let mut config = Configuration::new("Test");
        config.add_common_module(CommonModule::builder().name("ОбщегоНазначенияЁмкость").build());

        for spelling in
            ["ОбщегоНазначенияЁмкость", "общегоназначенияёмкость", "ОБЩЕГОНАЗНАЧЕНИЯЁМКОСТЬ"]
        {
            let found = config.find_common_module(spelling);
            assert!(found.is_some(), "no match for {spelling:?}");
            assert_eq!(found.unwrap().name(), "ОбщегоНазначенияЁмкость");
        }
        assert!(config.find_common_module("ОбщегоНазначенияЕмкость").is_none());
    }

    #[test]
    fn find_common_module_by_uri_lower_is_case_insensitive_and_first_wins() {
        let mut config = Configuration::new("Test");

        // Populated via the incremental `add_common_module` (disk-loader) path, not
        // `build_caches`, so the folded URI index must be filled there too.
        let first = CommonModule::builder()
            .name("Первый")
            .uri(Some("CommonModules/Первый/Ext/Module.bsl"))
            .return_values_reuse(ReturnValueReuse::DuringRequest)
            .build();
        config.add_common_module(first);

        // A second module colliding on the (lowercased) URI must not displace the first.
        let shadow = CommonModule::builder()
            .name("Второй")
            .uri(Some("commonmodules/первый/ext/module.bsl"))
            .return_values_reuse(ReturnValueReuse::DuringRequest)
            .build();
        config.add_common_module(shadow);

        let needle = "CommonModules/Первый/Ext/Module.bsl".fold_lower();
        let found = config.find_common_module_by_uri_lower(&needle);
        assert!(found.is_some(), "folded URI index must be populated via add_common_module");
        assert_eq!(found.unwrap().name(), "Первый", "first occurrence wins on URI collision");

        assert!(config.find_common_module_by_uri_lower("nope/missing.bsl").is_none());
    }

    #[test]
    fn test_find_child_by_uri() {
        let mut config = Configuration::new("Test");

        let module = CommonModule::builder()
            .name("Global")
            .uri(Some("CommonModules/Global/Ext/Module.bsl"))
            .global(true)
            .build();

        config.add_common_module(module);

        let child = config.find_child_by_uri("CommonModules/Global/Ext/Module.bsl");
        assert!(child.is_some());
        assert_eq!(child.unwrap().name(), "Global");
    }

    #[test]
    fn test_metadata_objects() {
        let mut config = Configuration::new("Test");

        let catalog = MetadataObject::new(MdoType::Catalog, "Номенклатура");
        config.add_metadata_object(catalog);

        assert_eq!(config.metadata_objects().len(), 1);
        assert!(config.has_metadata_object(MdoType::Catalog, "Номенклатура"));

        let found = config.find_metadata_object(MdoType::Catalog, "номенклатура");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "Номенклатура");
    }

    #[test]
    fn merge_extension_overlay_preserves_base_and_adds_extension_attributes() {
        use crate::enums::ObjectBelonging;

        let mut base = Configuration::new("Base");
        let mut base_catalog = MetadataObject::new(MdoType::Catalog, "Номенклатура");
        let base_uuid = uuid::Uuid::new_v4();
        base_catalog.set_uuid(base_uuid);
        base_catalog.add_attribute(crate::metadata_object::Attribute {
            name: "Родитель".to_string(),
            name_en: Some("Parent".to_string()),
            attr_type: AttributeType::Ref {
                mdo_type: MdoType::Catalog,
                name: "Номенклатура".to_string(),
            },
        });
        base.add_metadata_object(base_catalog);

        let mut extension = Configuration::new("Extension");
        let mut extension_catalog = MetadataObject::new(MdoType::Catalog, "Номенклатура");
        extension_catalog.set_object_belonging(ObjectBelonging::Adopted);
        extension_catalog.set_extends_uuid(base_uuid);
        extension_catalog.add_attribute(crate::metadata_object::Attribute {
            name: "БУС_Артикул".to_string(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(25) },
        });
        extension.add_metadata_object(extension_catalog);

        let merged = base.merged_with_extension(&extension);
        let catalog =
            merged.find_metadata_object(MdoType::Catalog, "Номенклатура").expect("merged catalog");

        assert!(catalog.find_attribute("Родитель").is_some());
        assert!(catalog.find_attribute("БУС_Артикул").is_some());
    }

    #[test]
    fn adopted_common_module_inherits_omitted_reuse_but_own_same_name_does_not() {
        use crate::enums::{ObjectBelonging, ReturnValueReuse};

        let mut base = Configuration::new("Base");
        let base_module = CommonModule::builder()
            .name("Shared")
            .server(true)
            .global(true)
            .client_managed_application(true)
            .client_ordinary_application(true)
            .external_connection(true)
            .server_call(true)
            .privileged(true)
            .return_values_reuse(ReturnValueReuse::DontUse)
            .build();
        let base_uuid = *base_module.uuid();
        base.add_common_module(base_module);

        let mut adopted = Configuration::new("Adopted");
        adopted.add_common_module(
            CommonModule::builder()
                .name("Shared")
                .object_belonging(ObjectBelonging::Adopted)
                .extends_uuid(base_uuid)
                .build(),
        );
        let inherited = base.merged_with_extension(&adopted);
        let inherited_module = inherited.find_common_module("Shared").unwrap();
        assert_eq!(inherited_module.return_values_reuse(), ReturnValueReuse::DontUse);
        assert!(inherited_module.is_server());
        assert!(inherited_module.is_global());
        assert!(inherited_module.is_client_managed_application());
        assert!(inherited_module.is_client_ordinary_application());
        assert!(inherited_module.is_external_connection());
        assert!(inherited_module.is_server_call());
        assert!(inherited_module.is_privileged());

        let mut explicit = Configuration::new("Explicit");
        explicit.add_common_module(
            CommonModule::builder()
                .name("Shared")
                .object_belonging(ObjectBelonging::Adopted)
                .extends_uuid(base_uuid)
                .return_values_reuse(ReturnValueReuse::DuringSession)
                .build(),
        );
        let overridden = base.merged_with_extension(&explicit);
        assert_eq!(
            overridden.find_common_module("Shared").unwrap().return_values_reuse(),
            ReturnValueReuse::DuringSession,
        );

        let mut independent = Configuration::new("Independent");
        independent.add_common_module(CommonModule::builder().name("Shared").build());
        let not_merged = base.merged_with_extension(&independent);
        assert_eq!(not_merged.common_modules().len(), 2, "an own namesake remains independent");
        let own =
            not_merged.common_modules().iter().find(|module| module.uuid() != &base_uuid).unwrap();
        assert_eq!(own.return_values_reuse(), ReturnValueReuse::Unknown);
        assert!(!own.is_server());
        assert!(!own.is_global());
        assert!(!own.is_client_managed_application());
        assert!(!own.is_client_ordinary_application());
        assert!(!own.is_external_connection());
        assert!(!own.is_server_call());
        assert!(!own.is_privileged());
        let preserved_base =
            not_merged.common_modules().iter().find(|module| module.uuid() == &base_uuid).unwrap();
        assert!(preserved_base.is_server());
        assert_eq!(preserved_base.return_values_reuse(), ReturnValueReuse::DontUse);

        let mut wrong_target = Configuration::new("WrongTarget");
        wrong_target.add_common_module(
            CommonModule::builder()
                .name("Shared")
                .object_belonging(ObjectBelonging::Adopted)
                .extends_uuid(uuid::Uuid::new_v4())
                .build(),
        );
        let wrong_target_result = base.merged_with_extension(&wrong_target);
        assert_eq!(
            wrong_target_result.common_modules().len(),
            2,
            "an adopted namesake targeting another UUID must not merge"
        );
        assert!(wrong_target_result
            .common_modules()
            .iter()
            .any(|module| module.uuid() == &base_uuid && module.is_server()));
        assert!(wrong_target_result.common_modules().iter().any(|module| {
            module.uuid() != &base_uuid
                && !module.is_server()
                && module.return_values_reuse() == ReturnValueReuse::Unknown
        }));
    }

    #[test]
    fn adopted_metadata_object_merges_by_uuid_and_same_name_own_object_stays_independent() {
        use crate::enums::ObjectBelonging;
        use crate::metadata_object::Attribute;

        let base_uuid = uuid::Uuid::new_v4();
        let mut base = Configuration::new("Base");
        let mut document = MetadataObject::new(MdoType::Document, "Заказ");
        document.set_uuid(base_uuid);
        let mut base_lines =
            crate::tabular_section::TabularSection::new(uuid::Uuid::new_v4(), "Товары");
        base_lines.set_attributes(vec![crate::tabular_section::TabularSectionAttribute::new(
            uuid::Uuid::new_v4(),
            "Номенклатура",
            AttributeType::Unknown,
        )]);
        document.add_tabular_section(base_lines);
        document.add_attribute(Attribute {
            name: "Основание".into(),
            name_en: None,
            attr_type: AttributeType::Boolean,
        });
        base.add_metadata_object(document);

        let mut extension = Configuration::new("Extension");
        let mut adopted = MetadataObject::new(MdoType::Document, "Заказ");
        adopted.set_object_belonging(ObjectBelonging::Adopted);
        adopted.set_extends_uuid(base_uuid);
        let mut overlay_lines =
            crate::tabular_section::TabularSection::new(uuid::Uuid::new_v4(), "Товары");
        overlay_lines.set_attributes(vec![crate::tabular_section::TabularSectionAttribute::new(
            uuid::Uuid::new_v4(),
            "Характеристика",
            AttributeType::Unknown,
        )]);
        adopted.add_tabular_section(overlay_lines);
        adopted.add_attribute(Attribute {
            name: "НомерВнешнегоЗаказа".into(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(20) },
        });
        extension.add_metadata_object(adopted);

        let merged = base.merged_with_extension(&extension);
        let object = merged.find_metadata_object(MdoType::Document, "Заказ").unwrap();
        assert!(object.find_attribute("Основание").is_some());
        assert!(object.find_attribute("НомерВнешнегоЗаказа").is_some());
        let columns = object.tabular_sections[0]
            .attributes()
            .iter()
            .map(|attribute| attribute.name())
            .collect::<Vec<_>>();
        assert!(columns.contains(&"Номенклатура"));
        assert!(columns.contains(&"Характеристика"));

        let mut independent = Configuration::new("Independent");
        let mut own = MetadataObject::new(MdoType::Document, "Заказ");
        own.add_attribute(Attribute {
            name: "СвоеПоле".into(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(20) },
        });
        independent.add_metadata_object(own);
        let separate = base.merged_with_extension(&independent);
        assert_eq!(separate.metadata_objects().len(), 2);
        assert!(separate.metadata_objects()[0].find_attribute("СвоеПоле").is_none());
        assert!(separate.metadata_objects()[1].find_attribute("Основание").is_none());
        assert!(separate
            .find_metadata_object(MdoType::Document, "Заказ")
            .unwrap()
            .find_attribute("СвоеПоле")
            .is_some());
        let mut wrong_target = Configuration::new("WrongTarget");
        let mut wrong_document = MetadataObject::new(MdoType::Document, "Заказ");
        wrong_document.set_object_belonging(ObjectBelonging::Adopted);
        wrong_document.set_extends_uuid(uuid::Uuid::new_v4());
        wrong_document.add_attribute(Attribute {
            name: "ЧужоеПоле".into(),
            name_en: None,
            attr_type: AttributeType::Boolean,
        });
        wrong_target.add_metadata_object(wrong_document);
        let wrong_target_result = base.merged_with_extension(&wrong_target);
        assert_eq!(
            wrong_target_result.metadata_objects().len(),
            2,
            "an adopted namesake targeting another UUID must remain independent"
        );
        assert!(wrong_target_result.metadata_objects().iter().all(|object| {
            !(object.find_attribute("Основание").is_some()
                && object.find_attribute("ЧужоеПоле").is_some())
        }));

        let second_extension = Configuration::new("SecondExtension");
        let after_second_merge = separate.merged_with_extension(&second_extension);
        assert!(
            after_second_merge
                .find_metadata_object(MdoType::Document, "Заказ")
                .unwrap()
                .find_attribute("СвоеПоле")
                .is_some(),
            "a later unrelated extension must preserve the earlier effective object"
        );
    }

    /// Enums and constants are parsed outside the shared MDO property reader, so
    /// they must carry their extension ownership too: an adopted copy that lost it
    /// would replace the base object instead of extending it.
    #[test]
    fn adopted_enum_and_constant_parsed_from_xml_extend_the_base_object() {
        use crate::xml_parser::{parse_constant_xml, parse_enum_xml};

        fn mdo(kind: &str, uuid: &str, name: &str, ownership: &str, body: &str) -> String {
            format!(
                r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" xmlns:v8="http://v8.1c.ru/8.1/data/core" xmlns:xs="http://www.w3.org/2001/XMLSchema"><{kind} uuid="{uuid}"><Properties><Name>{name}</Name>{ownership}</Properties>{body}</{kind}></MetaDataObject>"#
            )
        }
        let base_uuid = "11111111-1111-1111-1111-111111111111";
        let adopted = format!(
            "<ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>{base_uuid}</ExtendedConfigurationObject>"
        );
        let value = |name: &str| {
            format!(
                r#"<ChildObjects><EnumValue uuid="{name}"><Properties><Name>{name}</Name></Properties></EnumValue></ChildObjects>"#
            )
        };

        let mut base = Configuration::new("Base");
        base.add_metadata_object(
            parse_enum_xml(&mdo("Enum", base_uuid, "Статусы", "", &value("А"))).unwrap(),
        );
        base.add_metadata_object(
            parse_constant_xml(&mdo(
                "Constant",
                base_uuid,
                "Флаг",
                "<Type><v8:Type>xs:boolean</v8:Type></Type>",
                "",
            ))
            .unwrap(),
        );

        let ext_uuid = "22222222-2222-2222-2222-222222222222";
        let mut extension = Configuration::new("Extension");
        extension.add_metadata_object(
            parse_enum_xml(&mdo("Enum", ext_uuid, "Статусы", &adopted, &value("В"))).unwrap(),
        );
        extension.add_metadata_object(
            parse_constant_xml(&mdo("Constant", ext_uuid, "Флаг", &adopted, "")).unwrap(),
        );

        let merged = base.merged_with_extension(&extension);
        assert_eq!(merged.metadata_objects().len(), 2, "adopted objects must not be duplicated");
        let statuses = merged.find_metadata_object(MdoType::Enum, "Статусы").unwrap();
        assert!(statuses.find_enum_value("А").is_some(), "base enum value must survive");
        assert!(statuses.find_enum_value("В").is_some(), "extension enum value must be added");
        assert_eq!(
            merged.find_metadata_object(MdoType::Constant, "Флаг").unwrap().constant_type,
            Some(AttributeType::Boolean),
            "base constant type must survive an adopted copy without <Type>"
        );
    }

    #[test]
    fn extension_metadata_whole_configuration_merges_document_and_tabular_fields() {
        use crate::enums::ObjectBelonging;
        use crate::metadata_object::Attribute;
        use crate::tabular_section::{TabularSection, TabularSectionAttribute};
        use uuid::Uuid;

        let mut base = Configuration::new("Base");
        let base_document_uuid = Uuid::new_v4();
        let mut base_document = MetadataObject::new(MdoType::Document, "Заказ");
        base_document.set_uuid(base_document_uuid);
        base_document.add_attribute(Attribute {
            name: "БазовыйРеквизит".to_string(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(20) },
        });
        let mut base_goods = TabularSection::new(Uuid::new_v4(), "Товары");
        base_goods.set_name_en(Some("BaseGoods".to_string()));
        base_goods.set_synonym(Some("Базовые товары".to_string()));
        base_goods.set_use_mode(Some("ForItem".to_string()));
        base_goods.set_attributes(vec![
            TabularSectionAttribute::new(
                Uuid::new_v4(),
                "Номенклатура",
                AttributeType::String { length: Some(50) },
            ),
            TabularSectionAttribute::new(
                Uuid::new_v4(),
                "Количество",
                AttributeType::Number { precision: 10, scale: 0 },
            ),
        ]);
        base_document.add_tabular_section(base_goods);
        base.add_metadata_object(base_document);

        let mut without_section = Configuration::new("WithoutSection");
        let mut doc_without_section = MetadataObject::new(MdoType::Document, "Заказ");
        doc_without_section.set_object_belonging(ObjectBelonging::Adopted);
        doc_without_section.set_extends_uuid(base_document_uuid);
        doc_without_section.add_attribute(Attribute {
            name: "РасшРеквизит".to_string(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(30) },
        });
        without_section.add_metadata_object(doc_without_section);
        let inherited = base.merged_with_extension(&without_section);
        let inherited_goods = inherited
            .find_metadata_object(MdoType::Document, "Заказ")
            .unwrap()
            .find_tabular_section("Товары")
            .unwrap();
        assert_eq!(
            inherited_goods
                .attributes()
                .iter()
                .map(|attribute| (attribute.name(), attribute.attr_type()))
                .collect::<Vec<_>>(),
            [
                ("Номенклатура", &AttributeType::String { length: Some(50) }),
                ("Количество", &AttributeType::Number { precision: 10, scale: 0 }),
            ],
            "an extension without the borrowed section keeps the exact base shape"
        );

        let mut extension = Configuration::new("Extension");
        let mut extension_document = MetadataObject::new(MdoType::Document, "Заказ");
        extension_document.set_object_belonging(ObjectBelonging::Adopted);
        extension_document.set_extends_uuid(base_document_uuid);
        extension_document.add_attribute(Attribute {
            name: "РасшРеквизит".to_string(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(30) },
        });
        let mut extension_goods = TabularSection::new(Uuid::new_v4(), "Товары");
        extension_goods.set_name_en(Some("ExtensionGoods".to_string()));
        extension_goods.set_synonym(Some("Товары расширения".to_string()));
        extension_goods.set_use_mode(Some("ForFolder".to_string()));
        extension_goods.set_attributes(vec![
            TabularSectionAttribute::new(
                Uuid::new_v4(),
                "Количество",
                AttributeType::String { length: Some(15) },
            ),
            TabularSectionAttribute::new(
                Uuid::new_v4(),
                "РасшПоле",
                AttributeType::String { length: Some(25) },
            ),
        ]);
        extension_document.add_tabular_section(extension_goods);
        let mut new_section = TabularSection::new(Uuid::new_v4(), "РасшТаблица");
        new_section.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "Добавлено",
            AttributeType::String { length: None },
        )]);
        extension_document.add_tabular_section(new_section);
        extension.add_metadata_object(extension_document);

        let merged = base.merged_with_extension(&extension);
        let document = merged.find_metadata_object(MdoType::Document, "Заказ").unwrap();
        assert!(document.find_attribute("БазовыйРеквизит").is_some());
        assert!(document.find_attribute("РасшРеквизит").is_some());
        let goods = document.find_tabular_section("Товары").unwrap();
        assert_eq!(goods.name_en(), Some("ExtensionGoods"));
        assert_eq!(goods.synonym(), Some("Товары расширения"));
        assert_eq!(goods.use_mode(), Some("ForFolder"));
        assert_eq!(
            goods.attributes().iter().map(TabularSectionAttribute::name).collect::<Vec<_>>(),
            ["Номенклатура", "Количество", "РасшПоле"]
        );
        assert_eq!(
            goods.attributes()[0].attr_type(),
            &AttributeType::String { length: Some(50) },
            "the inherited base column keeps its exact type"
        );
        assert!(matches!(
            goods.attributes()[1].attr_type(),
            AttributeType::String { length: Some(15) }
        ));
        assert_eq!(
            goods.attributes().iter().filter(|attribute| attribute.name() == "Количество").count(),
            1,
            "the overlay replacement must not duplicate a same-named field"
        );
        let added = document.find_tabular_section("РасшТаблица").unwrap();
        assert_eq!(added.attributes().len(), 1);
        assert_eq!(added.attributes()[0].name(), "Добавлено");
        assert_eq!(added.attributes()[0].attr_type(), &AttributeType::String { length: None });

        for only in 0..3 {
            let mut base_section = TabularSection::new(Uuid::new_v4(), "Товары");
            base_section.set_name_en(Some("BaseGoods".to_string()));
            base_section.set_synonym(Some("Базовые товары".to_string()));
            base_section.set_use_mode(Some("ForItem".to_string()));
            let base_uuid = Uuid::new_v4();
            let mut base_document = MetadataObject::new(MdoType::Document, "Заказ");
            base_document.set_uuid(base_uuid);
            base_document.add_tabular_section(base_section);
            let mut base_config = Configuration::new("Base");
            base_config.add_metadata_object(base_document);

            let mut overlay_section = TabularSection::new(Uuid::new_v4(), "Товары");
            match only {
                0 => overlay_section.set_name_en(Some("ExtensionGoods".to_string())),
                1 => overlay_section.set_synonym(Some("Товары расширения".to_string())),
                _ => overlay_section.set_use_mode(Some("ForFolder".to_string())),
            }
            let mut overlay_document = MetadataObject::new(MdoType::Document, "Заказ");
            overlay_document.set_object_belonging(ObjectBelonging::Adopted);
            overlay_document.set_extends_uuid(base_uuid);
            overlay_document.add_tabular_section(overlay_section);
            let mut overlay_config = Configuration::new("Extension");
            overlay_config.add_metadata_object(overlay_document);

            let merged = base_config.merged_with_extension(&overlay_config);
            let section = merged
                .find_metadata_object(MdoType::Document, "Заказ")
                .unwrap()
                .find_tabular_section("Товары")
                .unwrap();
            assert_eq!(
                section.name_en(),
                Some(if only == 0 { "ExtensionGoods" } else { "BaseGoods" })
            );
            assert_eq!(
                section.synonym(),
                Some(if only == 1 {
                    "Товары расширения"
                } else {
                    "Базовые товары"
                })
            );
            assert_eq!(section.use_mode(), Some(if only == 2 { "ForFolder" } else { "ForItem" }));
        }
    }

    #[test]
    fn extension_metadata_merge_is_cyrillic_case_insensitive() {
        use crate::enums::ObjectBelonging;
        use crate::metadata_object::Attribute;
        use crate::tabular_section::{TabularSection, TabularSectionAttribute};

        let document_uuid = Uuid::new_v4();
        let common_module_uuid = Uuid::new_v4();
        let mut base_document = MetadataObject::new(MdoType::Document, "Заказ");
        base_document.set_uuid(document_uuid);
        let mut base_section = TabularSection::new(Uuid::new_v4(), "Товары");
        base_section.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "Номенклатура",
            AttributeType::String { length: Some(20) },
        )]);
        base_document.add_tabular_section(base_section);
        let mut base = Configuration::new("Base");
        base.add_metadata_object(base_document);
        base.add_common_module(
            CommonModule::builder()
                .uuid(common_module_uuid)
                .name("Сервер")
                .return_values_reuse(ReturnValueReuse::DontUse)
                .build(),
        );

        let mut overlay_document = MetadataObject::new(MdoType::Document, "заказ");
        overlay_document.set_object_belonging(ObjectBelonging::Adopted);
        overlay_document.set_extends_uuid(document_uuid);
        overlay_document.add_attribute(Attribute {
            name: "Расширение".to_string(),
            name_en: None,
            attr_type: AttributeType::Boolean,
        });
        let mut overlay_section = TabularSection::new(Uuid::new_v4(), "товары");
        overlay_section.set_attributes(vec![TabularSectionAttribute::new(
            Uuid::new_v4(),
            "номенклатура",
            AttributeType::Number { precision: 10, scale: 0 },
        )]);
        overlay_document.add_tabular_section(overlay_section);
        let mut extension = Configuration::new("Extension");
        extension.add_metadata_object(overlay_document);
        extension.add_common_module(
            CommonModule::builder()
                .name("сервер")
                .object_belonging(ObjectBelonging::Adopted)
                .extends_uuid(common_module_uuid)
                .return_values_reuse(ReturnValueReuse::DuringRequest)
                .build(),
        );

        let merged = base.merged_with_extension(&extension);
        assert_eq!(merged.metadata_objects().len(), 1, "Заказ and заказ are one object");
        assert_eq!(merged.common_modules().len(), 1, "Сервер and сервер are one module");
        let document = merged.find_metadata_object(MdoType::Document, "ЗАКАЗ").unwrap();
        assert!(document.find_attribute("РАСШИРЕНИЕ").is_some());
        assert_eq!(document.tabular_sections.len(), 1, "Товары and товары are one section");
        let section = document.find_tabular_section("ТОВАРЫ").unwrap();
        assert_eq!(section.attributes().len(), 1, "field casing must not create a duplicate");
        assert_eq!(
            section.attributes()[0].attr_type(),
            &AttributeType::Number { precision: 10, scale: 0 },
            "the case-variant overlay field replaces the base field"
        );
        assert_eq!(
            merged.find_common_module("СЕРВЕР").unwrap().return_values_reuse(),
            ReturnValueReuse::DuringRequest
        );
    }

    #[test]
    fn extension_metadata_xml_empty_borrowed_sections_preserve_base_shape() {
        let base_xml = include_str!("../fixtures/extension_metadata/base/Documents/Заказ.xml");
        let base_document = crate::xml_parser::parse_document_xml(base_xml).unwrap();
        let overlay_xml = |child_objects: &str| {
            format!(
                r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
<Document uuid="15500000-0000-0000-0000-000000000400">
<Properties><Name>Заказ</Name><ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>15500000-0000-0000-0000-000000000010</ExtendedConfigurationObject></Properties>
<ChildObjects><TabularSection uuid="15500000-0000-0000-0000-000000000401">
<Properties><Name>Товары</Name></Properties>{child_objects}
</TabularSection></ChildObjects></Document></MetaDataObject>"#
            )
        };

        for child_objects in ["", "<ChildObjects/>", "<ChildObjects></ChildObjects>"] {
            let overlay_document =
                crate::xml_parser::parse_document_xml(&overlay_xml(child_objects)).unwrap();
            let standalone = overlay_document.find_tabular_section("Товары").unwrap();
            assert!(standalone.attributes().is_empty(), "standalone empty section stays empty");

            let mut base = Configuration::new("Base");
            base.add_metadata_object(base_document.clone());
            let mut extension = Configuration::new("Extension");
            extension.add_metadata_object(overlay_document);
            let merged = base.merged_with_extension(&extension);
            let goods = merged
                .find_metadata_object(MdoType::Document, "Заказ")
                .unwrap()
                .find_tabular_section("Товары")
                .unwrap();
            assert_eq!(goods.attributes().len(), 2);
            assert_eq!(
                goods.attributes()[0].attr_type(),
                &AttributeType::String { length: Some(50) }
            );
            assert_eq!(
                goods.attributes()[1].attr_type(),
                &AttributeType::Number { precision: 10, scale: 0 }
            );
            assert_eq!(goods.synonym(), Some("Товары базы"));
            assert_eq!(goods.use_mode(), Some("ForItem"));
        }

        let overlay_without_sections = crate::xml_parser::parse_document_xml(
            r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses">
<Document uuid="15500000-0000-0000-0000-000000000402">
<Properties><Name>Заказ</Name><ObjectBelonging>Adopted</ObjectBelonging><ExtendedConfigurationObject>15500000-0000-0000-0000-000000000010</ExtendedConfigurationObject></Properties><ChildObjects/>
</Document></MetaDataObject>"#,
        )
        .unwrap();
        let mut base = Configuration::new("Base");
        base.add_metadata_object(base_document);
        let mut extension = Configuration::new("Extension");
        extension.add_metadata_object(overlay_without_sections);
        let merged = base.merged_with_extension(&extension);
        let goods = merged
            .find_metadata_object(MdoType::Document, "Заказ")
            .unwrap()
            .find_tabular_section("Товары")
            .unwrap();
        assert_eq!(
            goods
                .attributes()
                .iter()
                .map(|attribute| (attribute.name(), attribute.attr_type()))
                .collect::<Vec<_>>(),
            [
                ("Номенклатура", &AttributeType::String { length: Some(50) }),
                ("Количество", &AttributeType::Number { precision: 10, scale: 0 }),
            ],
            "a parsed document overlay without any tabular section keeps exact base types"
        );
    }

    #[test]
    fn merge_extension_overlay_merges_borrowed_register_fields() {
        use crate::dimension::Dimension;
        use crate::register::{Register, RegisterAttribute, RegisterResource};
        use uuid::Uuid;

        let mut base = Configuration::new("Base");
        base.add_register(
            Register::builder()
                .name("РегистрСведений1")
                .mdo_type(MdoType::InformationRegister)
                .add_dimension(Dimension::builder().name("Изм1").build())
                .add_resource(RegisterResource::new(Uuid::new_v4(), "Рес1"))
                .build(),
        );

        let mut extension = Configuration::new("Extension");
        extension.add_register(
            Register::builder()
                .name("РегистрСведений1")
                .mdo_type(MdoType::InformationRegister)
                .add_dimension(Dimension::builder().name("Изм2").build())
                .add_resource(RegisterResource::new(Uuid::new_v4(), "Рес2"))
                .add_attribute(RegisterAttribute::new(Uuid::new_v4(), "Рекв1"))
                .build(),
        );

        let merged = base.merged_with_extension(&extension);
        let reg = merged
            .find_register_by_type_and_name(MdoType::InformationRegister, "РегистрСведений1")
            .expect("merged register");

        // An extension that borrows the register adds its measurement, resource and
        // attribute — the base's own fields are preserved (not replaced wholesale).
        let dims: Vec<&str> = reg.dimensions().iter().map(|d| d.name()).collect();
        let res: Vec<&str> = reg.resources().iter().map(|r| r.name()).collect();
        let attrs: Vec<&str> = reg.attributes().iter().map(|a| a.name()).collect();
        assert_eq!(dims, ["Изм1", "Изм2"], "base + extension measurements");
        assert_eq!(res, ["Рес1", "Рес2"], "base + extension resources");
        assert_eq!(attrs, ["Рекв1"], "extension attribute added to the borrowed register");
    }

    #[test]
    fn merge_extension_overlay_refines_borrowed_defined_type() {
        use crate::defined_type::DefinedType;
        use uuid::Uuid;

        let mut base = Configuration::new("Base");
        base.add_defined_type(
            DefinedType::builder()
                .uuid(Uuid::new_v4())
                .name("ОпределяемыйТип1")
                .underlying_type(AttributeType::String { length: Some(10) })
                .build(),
        );

        let refined = AttributeType::Ref {
            mdo_type: MdoType::Catalog,
            name: "Номенклатура".into(),
        };
        let mut extension = Configuration::new("Extension");
        extension.add_defined_type(
            DefinedType::builder()
                .uuid(Uuid::new_v4())
                .name("ОпределяемыйТип1")
                .underlying_type(refined.clone())
                .build(),
        );

        let merged = base.merged_with_extension(&extension);
        let dt = merged.find_defined_type("ОпределяемыйТип1").expect("merged defined type");
        assert_eq!(dt.underlying_type(), &refined, "extension refinement of the defined type wins");
    }

    #[test]
    fn equality_reflects_subsystem_changes() {
        let base = Configuration::new("Cfg");

        let mut with_subsystem = base.clone();
        with_subsystem.add_subsystem(crate::subsystem::Subsystem::new("Продажи"));

        // A subsystem-only difference must be observable: Salsa relies on this
        // equality to decide whether a reload invalidates downstream consumers.
        assert_ne!(base, with_subsystem);

        let mut also_with_subsystem = base.clone();
        also_with_subsystem.add_subsystem(crate::subsystem::Subsystem::new("Продажи"));
        assert_eq!(with_subsystem, also_with_subsystem);
    }

    #[test]
    fn merge_extension_overlay_merges_subsystem_content_and_children() {
        let mut base = Configuration::new("Base");
        base.add_subsystem(
            crate::subsystem::Subsystem::new("Продажи")
                .with_content(vec![(MdoType::Catalog, "Номенклатура".to_string())])
                .with_child_subsystems(vec!["Договоры".to_string()]),
        );

        let mut extension = Configuration::new("Extension");
        extension.add_subsystem(
            crate::subsystem::Subsystem::new("продажи")
                .with_content(vec![(MdoType::Document, "ЗаказПокупателя".to_string())])
                .with_child_subsystems(vec!["договоры".to_string(), "Отчеты".to_string()]),
        );
        extension.add_subsystem(crate::subsystem::Subsystem::new("Сервис"));

        let merged = base.merged_with_extension(&extension);
        let sales = merged
            .subsystems()
            .iter()
            .find(|subsystem| subsystem.name().fold_lower() == "Продажи".fold_lower())
            .expect("merged subsystem");

        assert_eq!(
            sales.content(),
            &[
                (MdoType::Catalog, "Номенклатура".to_string()),
                (MdoType::Document, "ЗаказПокупателя".to_string()),
            ]
        );
        assert_eq!(sales.child_subsystems(), &["Договоры".to_string(), "Отчеты".to_string()]);
        assert!(merged
            .subsystems()
            .iter()
            .any(|subsystem| subsystem.name().fold_lower() == "Сервис".fold_lower()));
    }

    #[test]
    fn test_add_and_find_register() {
        use crate::register::Register;

        let mut config = Configuration::new("Test");

        let register = Register::builder()
            .name("РегистрСведений1")
            .mdo_type(MdoType::InformationRegister)
            .build();

        config.add_register(register);

        assert_eq!(config.registers().len(), 1);

        let found = config.find_register("РегистрСведений1");
        assert!(found.is_some());
        assert_eq!(found.unwrap().name(), "РегистрСведений1");

        let found_ci = config.find_register("регистрсведений1");
        assert!(found_ci.is_some());

        let found_typed =
            config.find_register_by_type_and_name(MdoType::InformationRegister, "РегистрСведений1");
        assert!(found_typed.is_some());

        let not_found = config
            .find_register_by_type_and_name(MdoType::AccumulationRegister, "РегистрСведений1");
        assert!(not_found.is_none());
    }

    #[test]
    fn recorders_for_register_indexes_document_register_records() {
        let mut config = Configuration::new("Test");
        let mut document = MetadataObject::new(MdoType::Document, "Документ1");
        document.set_register_records(vec![
            (MdoType::InformationRegister, "РегистрСведений1".to_string()),
            (MdoType::AccumulationRegister, "РегистрНакопления1".to_string()),
        ]);

        config.add_metadata_object(document);

        assert_eq!(
            config.recorders_for_register(MdoType::InformationRegister, "РегистрСведений1"),
            &["Документ1".to_string()],
        );
        assert_eq!(
            config.recorders_for_register(MdoType::AccumulationRegister, "регистрнакопления1"),
            &["Документ1".to_string()],
        );
        assert!(config
            .recorders_for_register(MdoType::AccountingRegister, "РегистрБухгалтерии1")
            .is_empty());
    }

    #[test]
    fn overlay_merge_rebuilds_recorder_cache() {
        use crate::enums::ObjectBelonging;

        let doc_name = "Документ1";
        let mut base = Configuration::new("Base");
        let mut base_document = MetadataObject::new(MdoType::Document, doc_name);
        let base_uuid = uuid::Uuid::new_v4();
        base_document.set_uuid(base_uuid);
        base_document.set_register_records(vec![(MdoType::InformationRegister, "A".to_string())]);
        base.add_metadata_object(base_document);

        let mut extension = Configuration::new("Extension");
        let mut overlay_document = MetadataObject::new(MdoType::Document, doc_name);
        overlay_document.set_object_belonging(ObjectBelonging::Adopted);
        overlay_document.set_extends_uuid(base_uuid);
        overlay_document
            .set_register_records(vec![(MdoType::InformationRegister, "B".to_string())]);
        extension.add_metadata_object(overlay_document);

        let merged = base.merged_with_extension(&extension);

        assert_eq!(
            merged.recorders_for_register(MdoType::InformationRegister, "B"),
            &[doc_name.to_string()],
        );
        assert!(merged.recorders_for_register(MdoType::InformationRegister, "A").is_empty());
    }

    #[test]
    fn configuration_heap_counts_objects_and_name_indexes() {
        let mut config = Configuration::new("ТестоваяКонфигурация");
        let name_capacity = config.name.capacity();

        let mut catalog = MetadataObject::new(MdoType::Catalog, "Номенклатура");
        catalog.add_attribute(crate::metadata_object::Attribute {
            name: "Артикул".to_string(),
            name_en: None,
            attr_type: AttributeType::String { length: Some(25) },
        });
        let catalog_name_capacity = catalog.name.capacity();
        config.add_metadata_object(catalog);

        let module = CommonModule::builder().name("ОбщийМодуль1").global(true).build();
        // `name` is private, so probe via the public accessor: `.len()` is a safe
        // lower bound on the real (private) `.capacity()`.
        let module_name_floor = module.name().len();
        config.add_common_module(module);

        let owned_floor = name_capacity + catalog_name_capacity + module_name_floor;

        let bytes = config.estimated_heap_size();
        // At least the owned name strings on the objects themselves (the name
        // indexes and vec backing stores add further bytes on top); well under
        // 8 KiB for two small entries plus their lookup-table entries.
        assert!(bytes > owned_floor);
        assert!(bytes < 8 * 1024);
    }

    #[test]
    fn test_multiple_register_types() {
        use crate::register::Register;

        let mut config = Configuration::new("Test");

        let info_reg = Register::builder()
            .name("РегистрСведений1")
            .mdo_type(MdoType::InformationRegister)
            .build();

        let accum_reg = Register::builder()
            .name("РегистрНакопления1")
            .mdo_type(MdoType::AccumulationRegister)
            .build();

        config.add_register(info_reg);
        config.add_register(accum_reg);

        assert_eq!(config.registers().len(), 2);

        let info_found = config.find_register("РегистрСведений1");
        assert!(info_found.is_some());
        assert!(info_found.unwrap().is_information_register());

        let accum_found = config.find_register("РегистрНакопления1");
        assert!(accum_found.is_some());
        assert!(accum_found.unwrap().is_accumulation_register());
    }
}
