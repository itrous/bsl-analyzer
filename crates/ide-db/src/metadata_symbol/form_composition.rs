use std::path::PathBuf;
use std::sync::Arc;

use base_db::SourceDatabase;
use bsl_conventions::{ConventionalName, XML_EXTENSION};
use bsl_metadata::inspection_sources::{BinaryFingerprint, InspectionIdentity};
use bsl_metadata::xml_parser::form_inspection::{
    parse_managed_form_inspection, parse_prepared_ordinary_form_inspection, FormInspection,
};
use bsl_metadata::{Form, MdoType, ObjectBelonging};
use hir::metadata_symbol::{
    MetadataIdentityError, MetadataIdentityResolver, MetadataSymbolIdentity, MetadataSymbolPath,
};

use crate::metadata::{inspection::OrdinaryFormFingerprintsInput, RootKind};
use crate::{RootDatabase, RootDatabaseImpl};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) enum FormDescriptorKind {
    CommonForm,
    Form,
}

impl FormDescriptorKind {
    fn xml_name(self) -> &'static str {
        match self {
            Self::CommonForm => "CommonForm",
            Self::Form => "Form",
        }
    }
}

/// Proven parent declaration for a form of a configuration object.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct FormParent {
    pub kind: MdoType,
    pub name: hir::Name,
    pub identity: MetadataSymbolIdentity,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) enum FormPurpose {
    Rename(Option<FormParent>),
    Inspection,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(super) struct FormRequest {
    pub roots: Vec<(String, PathBuf)>,
    pub selected: PathBuf,
    pub directory: String,
    pub descriptor: String,
    pub managed: String,
    pub kind: FormDescriptorKind,
    pub purpose: FormPurpose,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormView {
    Managed(Form),
    Inspection(FormInspection),
}

impl FormView {
    fn has_base_form(&self) -> bool {
        match self {
            Self::Managed(form) => form.has_base_form,
            Self::Inspection(form) => form.has_base_snapshot,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FormProblem {
    Rename(MetadataIdentityError),
    DescriptorUnavailable,
    InvalidDescriptor(String),
    ContentUnavailable(String),
    UnprovedAdoption,
    Inspection(String),
}

impl FormProblem {
    fn rename_error(&self) -> MetadataIdentityError {
        match self {
            Self::Rename(error) => error.clone(),
            Self::DescriptorUnavailable | Self::InvalidDescriptor(_) => {
                MetadataIdentityError::MissingDeclaration
            }
            Self::ContentUnavailable(_) | Self::Inspection(_) => {
                MetadataIdentityError::UnreadSource
            }
            Self::UnprovedAdoption => MetadataIdentityError::UnprovedAdoption,
        }
    }

    fn inspection_code(&self) -> Option<&str> {
        match self {
            Self::DescriptorUnavailable => Some("form_descriptor_unavailable"),
            Self::ContentUnavailable(_) => Some("form_content_unavailable"),
            Self::UnprovedAdoption => Some("adopted_base_identity_unavailable"),
            Self::Inspection(code) => Some(code),
            Self::Rename(_) | Self::InvalidDescriptor(_) => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ProblemAt {
    origin: String,
    problem: FormProblem,
}

impl ProblemAt {
    fn new(origin: &str, problem: FormProblem) -> Self {
        Self { origin: origin.into(), problem }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct FormSource {
    identity: InspectionIdentity,
    view: FormView,
    inspection_problems: Vec<ProblemAt>,
}

impl FormSource {
    fn apply_overlay(&mut self, overlay: Self) {
        match (&mut self.view, &overlay.view) {
            (FormView::Managed(base), FormView::Managed(overlay)) => {
                base.apply_extension_overlay(overlay);
            }
            (FormView::Inspection(base), FormView::Inspection(overlay_view)) => {
                let inherited_origin = base.origins.first().cloned().unwrap_or_default();
                let previous_reasons = base.incomplete_reasons.len();
                base.apply_overlay(overlay_view);
                let inherited_end =
                    base.incomplete_reasons.len() - overlay_view.incomplete_reasons.len();
                self.inspection_problems.extend(
                    base.incomplete_reasons[previous_reasons..inherited_end].iter().map(|code| {
                        ProblemAt::new(&inherited_origin, FormProblem::Inspection(code.clone()))
                    }),
                );
                self.inspection_problems.extend(overlay.inspection_problems);
            }
            _ => unreachable!("an overlay uses the same representation as its base"),
        }
        self.identity = overlay.identity;
    }

    fn inspection_problem(&mut self, origin: &str, problem: FormProblem) {
        if let FormView::Inspection(view) = &mut self.view {
            if let Some(code) = problem.inspection_code() {
                view.incomplete_reasons.push(code.into());
            }
            self.inspection_problems.push(ProblemAt::new(origin, problem));
        }
    }
}

/// Composition of the visible form sources, with provenance for incomplete results.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EffectiveFormComposition {
    effective: Option<FormSource>,
    problems: Vec<ProblemAt>,
}

impl EffectiveFormComposition {
    /// The inspected descriptor and available content, if a representation could be read.
    pub fn inspection(&self) -> Option<(&InspectionIdentity, &FormInspection)> {
        let source = self.effective.as_ref()?;
        match &source.view {
            FormView::Inspection(view) => Some((&source.identity, view)),
            FormView::Managed(_) => None,
        }
    }

    /// Each incomplete inspection reason as a code, source origin and optional error detail.
    pub fn inspection_reasons(&self) -> impl Iterator<Item = (&str, &str, Option<&str>)> {
        self.problems
            .iter()
            .chain(self.effective.iter().flat_map(|source| &source.inspection_problems))
            .filter_map(|reason| {
                let code = reason.problem.inspection_code()?;
                let detail = match &reason.problem {
                    FormProblem::ContentUnavailable(detail) => Some(detail.as_str()),
                    _ => None,
                };
                Some((code, reason.origin.as_str(), detail))
            })
    }

    pub(super) fn rename_form(&self) -> Result<&Form, MetadataIdentityError> {
        if let Some(problem) = self.problems.first() {
            return Err(problem.problem.rename_error());
        }
        match self.effective.as_ref().map(|source| &source.view) {
            Some(FormView::Managed(form)) => Ok(form),
            _ => Err(MetadataIdentityError::MissingDeclaration),
        }
    }
}

impl RootDatabaseImpl {
    /// Compose a form for inspection within the roots selected by the caller.
    pub fn metadata_inspect_form(
        &self,
        roots: Vec<(String, PathBuf)>,
        container: &str,
        name: &str,
    ) -> Result<Arc<EffectiveFormComposition>, String> {
        let Some((_, selected)) = roots.last() else {
            return Ok(Arc::default());
        };
        let directory = format!("{container}/{name}");
        let result = compose_form(
            self,
            FormRequest {
                selected: selected.clone(),
                roots,
                descriptor: format!("{directory}.{XML_EXTENSION}"),
                managed: format!(
                    "{directory}/{}/{}",
                    ConventionalName::Ext.canonical(),
                    ConventionalName::FormXml.canonical()
                ),
                directory,
                kind: if container == "CommonForms" {
                    FormDescriptorKind::CommonForm
                } else {
                    FormDescriptorKind::Form
                },
                purpose: FormPurpose::Inspection,
            },
        );
        if let Some(error) = result.problems.iter().find_map(|reason| match &reason.problem {
            FormProblem::InvalidDescriptor(error) => Some(error.clone()),
            _ => None,
        }) {
            return Err(error);
        }
        Ok(result)
    }
}

pub(super) fn compose_form(
    db: &RootDatabaseImpl,
    request: FormRequest,
) -> Arc<EffectiveFormComposition> {
    if db.try_source_root_input(base_db::METADATA_SOURCE_ROOT).is_none() {
        return Arc::default();
    }
    // The initial creation of the optional fingerprint input changes the query key.
    effective_form_query(db, OrdinaryFormFingerprintsInput::try_get(db), request)
}

#[salsa::tracked(lru = 128, returns(clone))]
fn effective_form_query(
    db: &dyn RootDatabase,
    fingerprints: Option<OrdinaryFormFingerprintsInput>,
    request: FormRequest,
) -> Arc<EffectiveFormComposition> {
    let db = db.as_any().downcast_ref::<RootDatabaseImpl>().expect("prepared form source database");
    let snapshot = db.workspace_configs_snapshot();
    let rename = matches!(&request.purpose, FormPurpose::Rename(_));
    let ext = format!("{}/{}", request.directory, ConventionalName::Ext.canonical());
    let sidecar_path = format!(
        "{ext}/{}/{}",
        ConventionalName::Form.canonical(),
        ConventionalName::FormJson.canonical()
    );
    let binary_path = format!("{ext}/{}", ConventionalName::FormBin.canonical());
    let name = request.directory.rsplit('/').next().unwrap_or(&request.directory);
    let mut result = EffectiveFormComposition::default();
    let mut proved: Option<FormSource> = None;
    // An orphan can supply card content, but cannot attest the next overlay.
    let mut fragment = None;
    for (label, root) in &request.roots {
        let origin = format!("{label}:{}", request.directory);
        let mut report = |problem| result.problems.push(ProblemAt::new(&origin, problem));
        let is_extension = snapshot
            .paths
            .iter()
            .position(|(_, path)| path == root)
            .is_some_and(|index| snapshot.kinds.get(index) == Some(&RootKind::Extension));
        let descriptor_file = db.inspection_file_id(root, &request.descriptor);
        if rename && descriptor_file.is_none() {
            if root == &request.selected {
                report(FormProblem::DescriptorUnavailable);
                break;
            }
            continue;
        }
        if let (FormPurpose::Rename(Some(parent)), Some(file)) = (&request.purpose, descriptor_file)
        {
            let path = MetadataSymbolPath {
                owner_kind: parent.kind,
                owner_name: parent.name.clone(),
                members: Vec::new(),
            };
            match db.metadata_identity(file, &path) {
                Ok(identity) if identity == parent.identity => {}
                Ok(_) => continue,
                Err(error) => {
                    report(FormProblem::Rename(error));
                    break;
                }
            }
        }
        let descriptor = descriptor_file
            .filter(|&file| !db.file_is_unread(file))
            .map(|file| db.file_text_ref(file));
        if rename && descriptor.is_none() {
            report(FormProblem::Rename(MetadataIdentityError::UnreadSource));
            break;
        }
        let identity = descriptor.map(|text| {
            let kind = request.kind.xml_name();
            if is_extension {
                InspectionIdentity::from_extension_form_xml(text, kind)
            } else {
                InspectionIdentity::from_xml(text, kind)
            }
        });
        let identity = match identity.transpose() {
            Ok(identity) => identity,
            Err(error) => {
                report(FormProblem::InvalidDescriptor(error.to_string()));
                break;
            }
        };
        let form_type = identity.as_ref().and_then(|identity| identity.form_type.as_deref());
        if rename
            && (identity.as_ref().is_some_and(|identity| identity.uuid.is_nil())
                || !matches!(form_type, None | Some("Managed")))
        {
            report(FormProblem::Rename(MetadataIdentityError::UnreadSource));
            break;
        }
        let belonging = identity.as_ref().map_or(ObjectBelonging::Unknown, |i| i.object_belonging);
        let unknown_extension = is_extension && belonging == ObjectBelonging::Unknown;
        if rename && unknown_extension {
            report(FormProblem::UnprovedAdoption);
            break;
        }
        let managed = db.inspection_file(root, &request.managed);
        let view = if rename {
            managed
                .ok_or_else(|| "managed_representation_unavailable".into())
                .and_then(|text| {
                    bsl_metadata::xml_parser::parse_form_xml(&text)
                        .map_err(|error| error.to_string())
                })
                .and_then(|form| {
                    if form.is_managed() {
                        Ok(FormView::Managed(form))
                    } else {
                        Err("managed_representation_unavailable".into())
                    }
                })
        } else {
            let sidecar = db.inspection_file(root, &sidecar_path);
            if descriptor_file.is_none() && managed.is_none() && sidecar.is_none() {
                continue;
            }
            inspect_form(form_type, managed.as_deref(), sidecar.as_deref(), name, &origin, || {
                fingerprints.and_then(|_| db.ordinary_form_fingerprint(root, &binary_path))
            })
            .map(FormView::Inspection)
        };
        if unknown_extension {
            fragment = None;
            report(FormProblem::UnprovedAdoption);
            if identity.is_none() {
                report(FormProblem::DescriptorUnavailable);
            }
            if let Err(error) = view {
                report(FormProblem::ContentUnavailable(error));
            }
            continue;
        }
        let view = match view {
            Ok(view) => view,
            Err(error) => {
                report(FormProblem::ContentUnavailable(error));
                if rename {
                    break;
                }
                if form_type == Some("Ordinary") || belonging == ObjectBelonging::Own {
                    proved = None;
                    fragment = None;
                }
                continue;
            }
        };
        let inspection_problems = match &view {
            FormView::Managed(_) => Vec::new(),
            FormView::Inspection(view) => view
                .incomplete_reasons
                .iter()
                .map(|code| ProblemAt::new(&origin, FormProblem::Inspection(code.clone())))
                .collect(),
        };
        let Some(identity) = identity else {
            let FormView::Inspection(inspection) = &view else {
                unreachable!("a rename requires a descriptor");
            };
            let mut source = FormSource {
                identity: InspectionIdentity {
                    uuid: bsl_metadata::Uuid::nil(),
                    name: name.into(),
                    object_belonging: ObjectBelonging::Unknown,
                    extends: None,
                    form_type: Some(inspection.form_type.clone()),
                    template_type: None,
                },
                view,
                inspection_problems,
            };
            source.inspection_problem(&origin, FormProblem::DescriptorUnavailable);
            proved = None;
            fragment = Some(source);
            continue;
        };
        let mut source = FormSource { identity, view, inspection_problems };
        if belonging == ObjectBelonging::Adopted {
            if let Some(base) =
                proved.as_mut().filter(|base| source.identity.adopts(&base.identity))
            {
                base.apply_overlay(source);
                fragment = None;
            } else {
                if rename {
                    report(FormProblem::UnprovedAdoption);
                    break;
                }
                source.inspection_problem(&origin, FormProblem::UnprovedAdoption);
                fragment = Some(source);
            }
        } else if source.view.has_base_form() {
            report(FormProblem::UnprovedAdoption);
            fragment = None;
            if rename {
                break;
            }
        } else {
            proved = Some(source);
            fragment = None;
        }
    }
    result.effective = fragment.or(proved);
    Arc::new(result)
}

fn inspect_form(
    form_type: Option<&str>,
    managed: Option<&str>,
    sidecar: Option<&str>,
    name: &str,
    origin: &str,
    fingerprint: impl FnOnce() -> Option<BinaryFingerprint>,
) -> Result<FormInspection, String> {
    match (form_type, managed) {
        (Some("Managed"), None) => Err("managed_representation_unavailable".into()),
        (Some("Ordinary"), _) | (_, None) => {
            let sidecar =
                sidecar.ok_or_else(|| "ordinary_representation_unavailable".to_string())?;
            parse_prepared_ordinary_form_inspection(sidecar, name, fingerprint().as_ref(), origin)
        }
        (_, Some(text)) => {
            parse_managed_form_inspection(text, origin).map_err(|error| error.to_string())
        }
    }
}
