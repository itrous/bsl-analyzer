//! Bind semantic metadata paths to declarations in prepared, file-visible source sets.

mod form_bindings;

use base_db::SourceDatabase;
use bsl_metadata::declaration_identity::{
    metadata_declarations, DeclarationSegment, MetadataDeclaration,
};
use bsl_metadata::{ObjectBelonging, Uuid};
use hir::metadata_symbol::{
    MetadataIdentityError, MetadataIdentityResolver, MetadataSymbolIdentity, MetadataSymbolPath,
};
use std::sync::Arc;
use vfs::FileId;

use crate::metadata::config_index;
use crate::RootDatabaseImpl;

fn declaration_heap(value: &Result<Arc<Vec<MetadataDeclaration>>, MetadataIdentityError>) -> usize {
    value.as_ref().map_or(0, |declarations| {
        stdx::heap::vec_bytes::<MetadataDeclaration>(declarations.len())
            + declarations
                .iter()
                .map(|declaration| {
                    stdx::heap::vec_bytes::<DeclarationSegment>(declaration.path.len())
                        + declaration
                            .path
                            .iter()
                            .map(|segment| segment.kind.capacity() + segment.name.capacity())
                            .sum::<usize>()
                })
                .sum::<usize>()
    })
}

#[salsa::tracked(lru=8192,returns(clone),heap_size=declaration_heap)]
fn declaration_sites(
    db: &dyn base_db::SourceDatabase,
    file: base_db::FileIdInput<'_>,
) -> Result<Arc<Vec<MetadataDeclaration>>, MetadataIdentityError> {
    let id = file.file_id(db);
    if db.file_is_unread(id) {
        return Err(MetadataIdentityError::UnreadSource);
    }
    metadata_declarations(db.file_text_ref(id))
        .map(Arc::new)
        .map_err(|_| MetadataIdentityError::UnreadSource)
}

#[derive(Clone)]
struct BoundDeclaration {
    identity: MetadataSymbolIdentity,
    aliases: Vec<Uuid>,
}

impl MetadataIdentityResolver for RootDatabaseImpl {
    fn metadata_identity(
        &self,
        file_id: FileId,
        path: &MetadataSymbolPath,
    ) -> Result<MetadataSymbolIdentity, MetadataIdentityError> {
        let roots =
            self.metadata_symbol_roots_for_file(file_id, path.owner_kind).ok_or_else(|| {
                if crate::vfs_helpers::get_file_path(self, file_id).is_some_and(|path| {
                    matches!(
                        self.workspace_configs_snapshot().owner_kind_of_path(&path),
                        Some(crate::metadata::RootKind::External(_))
                    )
                }) {
                    MetadataIdentityError::UnprovedExternalOwner
                } else {
                    MetadataIdentityError::NoVisibleSource
                }
            })?;
        let snapshot = self.workspace_configs_snapshot();
        let roots = roots.into_iter().map(|root| {
            let is_extension =
                snapshot.paths.iter().position(|(_, path)| path == &root).is_some_and(|index| {
                    snapshot.kinds.get(index) == Some(&crate::metadata::RootKind::Extension)
                });
            (root, is_extension)
        });
        let mut prefixes: Vec<Option<BoundDeclaration>> = vec![None; path.members.len() + 1];
        let mut unproved = false;
        for (root, is_extension) in roots {
            let listing = self
                .metadata_listing(&root.to_string_lossy())
                .ok_or(MetadataIdentityError::UnreadSource)?;
            if listing
                .entries(self)
                .iter()
                .filter(|entry| {
                    entry.kind == path.owner_kind
                        && stdx::case::eq_ignore_case(&entry.name, path.owner_name.as_str())
                })
                .count()
                > 1
            {
                return Err(MetadataIdentityError::AmbiguousDeclaration);
            }
            let Some(files) =
                config_index(self, listing).lookup(path.owner_kind, path.owner_name.as_str())
            else {
                continue;
            };
            if self.file_is_unread(files.main) {
                return Err(MetadataIdentityError::UnreadSource);
            }
            let declarations =
                declaration_sites(self, base_db::FileIdInput::new(self, files.main))?;
            let mut owners = declarations.iter().filter(|declaration| {
                declaration.path.len() == 1
                    && declaration.path[0].kind == path.owner_kind.english_name()
                    && stdx::case::eq_ignore_case(
                        &declaration.path[0].name,
                        path.owner_name.as_str(),
                    )
            });
            let owner = owners.next().ok_or(MetadataIdentityError::MissingDeclaration)?;
            if owners.next().is_some() {
                return Err(MetadataIdentityError::AmbiguousDeclaration);
            }
            if is_extension && owner.object_belonging == ObjectBelonging::Unknown {
                return Err(MetadataIdentityError::UnprovedAdoption);
            }
            let adopted = owner.object_belonging == ObjectBelonging::Adopted;
            if adopted {
                let existing =
                    prefixes[0].as_mut().ok_or(MetadataIdentityError::UnprovedAdoption)?;
                if !owner.extends.is_some_and(|uuid| existing.aliases.contains(&uuid)) {
                    return Err(MetadataIdentityError::UnprovedAdoption);
                }
                if !existing.aliases.contains(&owner.uuid) {
                    existing.aliases.push(owner.uuid);
                }
            } else {
                prefixes.fill(None);
                unproved = false;
                prefixes[0] = Some(BoundDeclaration {
                    identity: MetadataSymbolIdentity {
                        declaration_file: files.main,
                        owner_uuid: owner.uuid,
                        declaration_uuid: owner.uuid,
                    },
                    aliases: vec![owner.uuid],
                });
            }
            for depth in 1..prefixes.len() {
                let mut matches = declarations.iter().filter(|declaration| {
                    declaration.path.first() == owner.path.first()
                        && matches_path(declaration, &path.members[..depth])
                });
                let Some(declaration) = matches.next() else { continue };
                if matches.next().is_some() {
                    return Err(MetadataIdentityError::AmbiguousDeclaration);
                }
                if prefixes[depth - 1].is_none() {
                    unproved = true;
                    continue;
                }
                let owner_uuid = prefixes[0]
                    .as_ref()
                    .expect("owner bound before its members")
                    .identity
                    .owner_uuid;
                let inherited = prefixes[depth].as_mut();
                match (declaration.object_belonging, inherited) {
                    (ObjectBelonging::Adopted, Some(existing)) => {
                        if !declaration.extends.is_some_and(|uuid| existing.aliases.contains(&uuid))
                        {
                            unproved = true;
                            continue;
                        }
                        if !existing.aliases.contains(&declaration.uuid) {
                            existing.aliases.push(declaration.uuid);
                        }
                    }
                    (ObjectBelonging::Adopted, None) => {
                        unproved = true;
                    }
                    (ObjectBelonging::Unknown, Some(existing))
                        if adopted && existing.aliases.contains(&declaration.uuid) => {}
                    (ObjectBelonging::Unknown, _) if adopted => {
                        unproved = true;
                    }
                    (_, _) => {
                        for descendant in &mut prefixes[depth..] {
                            *descendant = None;
                        }
                        prefixes[depth] = Some(BoundDeclaration {
                            identity: MetadataSymbolIdentity {
                                declaration_file: files.main,
                                owner_uuid,
                                declaration_uuid: declaration.uuid,
                            },
                            aliases: vec![declaration.uuid],
                        });
                    }
                }
            }
        }
        if unproved {
            return Err(MetadataIdentityError::UnprovedAdoption);
        }
        prefixes
            .last()
            .and_then(Option::as_ref)
            .map(|bound| bound.identity.clone())
            .ok_or(MetadataIdentityError::MissingDeclaration)
    }
}

fn matches_path(declaration: &MetadataDeclaration, members: &[DeclarationSegment]) -> bool {
    declaration.path.len() == members.len() + 1
        && declaration.path[1..].iter().zip(members).all(|(source, requested)| {
            source.kind == requested.kind
                && stdx::case::eq_ignore_case(&source.name, &requested.name)
        })
}
