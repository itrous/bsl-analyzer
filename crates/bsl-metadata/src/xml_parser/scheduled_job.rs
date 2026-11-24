use crate::error::{MetadataError, Result};
use crate::scheduled_job::ScheduledJob;

use super::helpers::{child_bool, child_text, find_child, find_mdo_element, parse_uuid, parse_xml};

pub fn parse_scheduled_job_xml(xml: &str) -> Result<ScheduledJob> {
    let _span = tracing::debug_span!("parse_scheduled_job_xml").entered();

    let doc = parse_xml(xml)?;
    let mdo = find_mdo_element(&doc)
        .ok_or_else(|| MetadataError::InvalidFormat("No ScheduledJob element found".to_string()))?;

    let uuid_str = mdo.attribute("uuid").unwrap_or("");
    let uuid = parse_uuid(uuid_str, "scheduled job")?;

    let props = find_child(mdo, "Properties").ok_or_else(|| {
        MetadataError::InvalidFormat("ScheduledJob missing Properties".to_string())
    })?;

    let job = ScheduledJob {
        uuid,
        name: child_text(props, "Name").unwrap_or("").to_string(),
        method_name: child_text(props, "MethodName").unwrap_or("").to_string(),
        predefined: child_bool(props, "Predefined"),
        use_flag: child_bool(props, "Use"),
        enabled: match child_text(props, "Use") {
            Some("true" | "1") => Some(true),
            Some("false" | "0") => Some(false),
            _ => None,
        },
        schedule: find_child(props, "Schedule").map(super::read_properties::parse_value),
    };

    tracing::debug!(
        job_name = %job.name(),
        uuid = %job.uuid,
        method_name = %job.method_name(),
        predefined = job.is_predefined(),
        "parsed scheduled job"
    );

    Ok(job)
}
