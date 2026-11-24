use super::*;
use bsl_conventions::{ConventionalName, XML_EXTENSION};
use bsl_metadata::rename_xml::XmlDumpInfoEntry;

struct SourceIdentity {
    path: Vec<DeclarationSegment>,
    uuid: bsl_metadata::Uuid,
    internal_object_ids: Vec<bsl_metadata::Uuid>,
}

fn declarations(snapshot: &PreparedMetadataRenameSnapshot, root: &str) -> Vec<SourceIdentity> {
    let mut sites = Vec::new();
    for file in snapshot
        .files
        .iter()
        .filter(|file| file.root_id == root && file.kind == MetadataPlanFileKind::Xml)
    {
        let Ok(source) = MetadataXmlSource::decode(&file.bytes) else { continue };
        let facts = source.inspect(None);
        for site in facts.declarations {
            let mut path = site.declaration.path;
            // Nested descriptor files carry their parent in the source layout, not in the XML root.
            if path
                .first()
                .is_some_and(|part| matches!(part.kind.as_str(), "Form" | "Template" | "Command"))
            {
                let parts =
                    file.relative_path.iter().filter_map(|part| part.to_str()).collect::<Vec<_>>();
                if let [collection, owner, children, filename] = parts.as_slice() {
                    let member = &path[0];
                    let child_kind = match *children {
                        name if name == ConventionalName::Forms.canonical() => "Form",
                        "Templates" => "Template",
                        name if name == ConventionalName::Commands.canonical() => "Command",
                        _ => "",
                    };
                    if child_kind == member.kind
                        && stdx::case::eq_ignore_case(
                            filename
                                .strip_suffix(XML_EXTENSION)
                                .and_then(|name| name.strip_suffix('.'))
                                .unwrap_or(filename),
                            &member.name,
                        )
                    {
                        if let Some(kind) =
                            bsl_metadata::module_path::collection_directory(collection)
                        {
                            path.insert(
                                0,
                                DeclarationSegment {
                                    kind: kind.english_name().into(),
                                    name: (*owner).into(),
                                },
                            );
                        }
                    }
                }
            }
            sites.push(SourceIdentity {
                path,
                uuid: site.declaration.uuid,
                internal_object_ids: site.internal_object_ids,
            });
        }
        for container in facts.container_identities {
            sites.push(SourceIdentity {
                path: vec![DeclarationSegment {
                    kind: "Configuration".into(),
                    name: container.name.text,
                }],
                uuid: container.uuid,
                internal_object_ids: container.internal_object_ids,
            });
        }
    }
    sites
}

fn prefix(entry: &XmlDumpInfoEntry, source: &SourceIdentity) -> bool {
    let path = &source.path;
    entry.names.len() >= path.len() * 2
        && path.iter().zip(entry.names.as_chunks::<2>().0).all(|(part, pair)| {
            stdx::case::eq_ignore_case(&part.kind, &pair[0].text)
                && stdx::case::eq_ignore_case(&part.name, &pair[1].text)
        })
}

fn attested(entry: &XmlDumpInfoEntry, source: &SourceIdentity) -> bool {
    if !prefix(entry, source) {
        return false;
    }
    let length = source.path.len() * 2;
    if entry.names.len() == length {
        return entry.id.parse::<bsl_metadata::Uuid>().ok() == Some(source.uuid);
    }
    let Some((id, suffix)) = entry.id.split_once('.') else {
        return false;
    };
    let Ok(uuid) = id.parse::<bsl_metadata::Uuid>() else {
        return false;
    };
    // The opaque suffix is never interpreted as a slot number or a version algorithm.
    !suffix.is_empty()
        && entry.names.len() == length + 1
        && matches!(
            entry.names[length].text.as_str(),
            "Module"
                | "ObjectModule"
                | "ManagerModule"
                | "Form"
                | "Help"
                | "Template"
                | "Predefined"
                | "CommandInterface"
                | "SessionModule"
                | "OrdinaryApplicationModule"
                | "ManagedApplicationModule"
                | "ExternalConnectionModule"
        )
        && (uuid == source.uuid || source.internal_object_ids.contains(&uuid))
}

#[allow(clippy::too_many_arguments)] // UUID attestation and raw byte edits use the same immutable snapshot.
pub(super) fn dump_info(
    db: &RootDatabaseImpl,
    file_id: FileId,
    file: &PreparedMetadataRenameFile,
    source: &MetadataXmlSource,
    facts: &XmlMetadataReferences,
    snapshot: &PreparedMetadataRenameSnapshot,
    spellings: &[Name],
    new_name: &str,
    edits: &mut Vec<MetadataRawEdit>,
    plan: &mut MetadataRenamePlan,
) {
    if facts.dump_info.is_empty() {
        return;
    }
    let sources = declarations(snapshot, &file.root_id);
    let mut by_id: std::collections::BTreeMap<bsl_metadata::Uuid, Vec<usize>> =
        std::collections::BTreeMap::new();
    for (index, source) in sources.iter().enumerate() {
        by_id.entry(source.uuid).or_default().push(index);
        for id in source.internal_object_ids.iter().filter(|id| **id != source.uuid) {
            by_id.entry(*id).or_default().push(index);
        }
    }
    for entry in &facts.dump_info {
        let id = entry
            .id
            .split_once('.')
            .map_or(entry.id.as_str(), |(id, _)| id)
            .parse::<bsl_metadata::Uuid>()
            .ok();
        let candidates = id.as_ref().and_then(|id| by_id.get(id));
        let sites = candidates
            .filter(|indices| indices.len() == 1)
            .into_iter()
            .flatten()
            .map(|index| &sources[*index])
            .filter(|site| attested(entry, site))
            .collect::<Vec<_>>();
        let [site] = sites.as_slice() else {
            plan.problems.push(MetadataPlanProblem::Xml {
                file_id,
                reason: XmlReferenceProblem::UnknownReferenceContract,
            });
            continue;
        };
        let Some(owner) = site.path.first() else {
            continue;
        };
        let Ok(kind) = owner.kind.parse() else {
            continue;
        };
        if !potential_owner(kind, &owner.name, plan, spellings) {
            continue;
        }
        let mut path = MetadataSymbolPath {
            owner_kind: kind,
            owner_name: Name::new(&owner.name),
            members: Vec::new(),
        };
        if matched(db, file_id, &path, plan) {
            add_token(&file.bytes, source, &entry.names[1], new_name, edits);
        }
        for (index, part) in site.path.iter().enumerate().skip(1) {
            if !matches!(
                part.kind.as_str(),
                "Attribute" | "TabularSection" | "Dimension" | "Resource" | "EnumValue"
            ) {
                break;
            }
            path.members.push(part.clone());
            if matched(db, file_id, &path, plan) {
                add_token(&file.bytes, source, &entry.names[index * 2 + 1], new_name, edits);
            }
        }
    }
}
