#![allow(missing_docs)]

pub mod common_attribute;
pub mod common_module;
pub mod configuration;
pub mod defined_type;
pub mod dimension;
pub mod enums;
pub mod error;
pub mod event_subscription;
pub mod external_object;
pub mod form;
pub mod http_service;
pub mod integration_service;
pub mod loader;
pub mod metadata_object;
pub mod metadata_resolver;
pub mod module_path;
pub mod register;
pub mod role;
pub mod scheduled_job;
pub mod scoped_fs;
pub mod subsystem;
pub mod tabular_section;
pub mod traits;
pub mod web_service;
pub mod xml_parser;

pub use common_attribute::{
    kind_takes_common_attributes, CommonAttribute, CommonAttributeField, CommonAttributeSet,
    CommonAttributeUse, ObjectCommonAttributes,
};
pub use common_module::{CommonModule, CommonModuleBuilder};
pub use configuration::Configuration;
pub use defined_type::{DefinedType, DefinedTypeBuilder};
pub use dimension::{Dimension, DimensionBuilder};
pub use enums::{CodeSeries, FormType, ModuleType, ObjectBelonging, ReturnValueReuse};
pub use error::{MetadataError, Result};
pub use event_subscription::{EventSubscription, EventSubscriptionHandler};
pub use external_object::{
    discover_external_object_structure, external_object_xml, load_external_object,
    ExternalObjectKind,
};
pub use form::{
    Form, FormAttribute, FormAttributeColumn, FormElement, FormElementKind, FormEventHandler,
};
pub use http_service::{
    HTTPService, HTTPServiceBuilder, HTTPServiceMethod, HTTPServiceMethodBuilder,
    HTTPServiceURLTemplate, HTTPServiceURLTemplateBuilder,
};
pub use integration_service::{
    IntegrationService, IntegrationServiceBuilder, IntegrationServiceChannel,
    IntegrationServiceChannelBuilder,
};
pub use loader::{
    discover_common_attribute_structure, discover_common_module_structure,
    discover_defined_type_structure, discover_event_subscription_structure,
    discover_http_service_structure, discover_integration_service_structure,
    discover_metadata_structure, discover_register_structure, discover_role_structure,
    discover_scheduled_job_structure, discover_subsystem_structure, discover_web_service_structure,
    load_from_directory, load_from_directory_scoped, parse_common_attribute_from_text,
    parse_common_module_from_text, parse_defined_type_from_text,
    parse_event_subscription_from_text, parse_http_service_from_text,
    parse_integration_service_from_text, parse_metadata_object_from_texts,
    parse_register_from_text, parse_role_from_texts, parse_scheduled_job_from_text,
    parse_subsystem_from_text, parse_web_service_from_text, DiscoveredCommonAttribute,
    DiscoveredCommonModule, DiscoveredDefinedType, DiscoveredEventSubscription,
    DiscoveredHTTPService, DiscoveredIntegrationService, DiscoveredMdo, DiscoveredRole,
    DiscoveredScheduledJob, DiscoveredSubsystem, DiscoveredWebService,
};
pub use metadata_object::{
    is_standard_attribute_name, Attribute, AttributeType, MdoType, MetadataObject, Name,
    PlatformValueType,
};
pub use metadata_resolver::{
    resolve_defined_type_terminal, MetadataResolver, QueryMetadataResolver,
};
pub use register::{
    AccumulationRegisterType, Register, RegisterAttribute, RegisterBuilder, RegisterPeriodicity,
    RegisterResource,
};
pub use role::{Role, RoleData, RoleObjectRef};
pub use roxmltree;
pub use scheduled_job::{ScheduledJob, ScheduledJobHandler};
pub use scoped_fs::ScopedFs;
pub use subsystem::Subsystem;
pub use tabular_section::{TabularSection, TabularSectionAttribute};
pub use traits::{MdObject, Module};
pub use uuid::Uuid;
pub use web_service::{
    WebService, WebServiceBuilder, WebServiceOperation, WebServiceOperationBuilder,
    WebServiceParameter,
};
