use super::*;

fn elements<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
) -> impl Iterator<Item = roxmltree::Node<'a, 'input>> {
    node.children().filter(|child| child.is_element())
}

fn single<'a, 'input>(
    node: roxmltree::Node<'a, 'input>,
    name: &str,
) -> Option<roxmltree::Node<'a, 'input>> {
    let mut children = elements(node).filter(|child| child.has_tag_name((DCS, name)));
    let child = children.next()?;
    children.next().is_none().then_some(child)
}

fn typed(node: roxmltree::Node<'_, '_>, expected: &str) -> bool {
    let Some(value) = node.attribute((XSI, "type")) else { return false };
    let (prefix, name) =
        value.split_once(':').map_or((None, value), |(prefix, name)| (Some(prefix), name));
    name == expected && node.lookup_namespace_uri(prefix) == Some(DCS)
}

fn leaf(node: roxmltree::Node<'_, '_>) -> bool {
    node.attributes().len() == 0 && elements(node).next().is_none()
}

fn field_shape(node: roxmltree::Node<'_, '_>) -> bool {
    let mut names = std::collections::BTreeSet::new();
    node.has_tag_name((DCS, "field"))
        && (node.attributes().len() == 0
            || (node.attributes().len() == 1 && typed(node, "DataSetFieldField")))
        && elements(node).all(|child| {
            child.tag_name().namespace() == Some(DCS)
                && names.insert(child.tag_name().name())
                && match child.tag_name().name() {
                    "dataPath" | "field" => {
                        leaf(child) && child.text().is_some_and(properties::identifier)
                    }
                    "title" => leaf(child),
                    "valueType" => {
                        leaf(child) && child.text().is_none_or(|text| text.trim().is_empty())
                    }
                    _ => false,
                }
        })
        && single(node, "dataPath").is_some()
        && single(node, "field").is_some()
}

impl MetadataXmlSource {
    pub(super) fn composition_expressions(
        &self,
        root: roxmltree::Node<'_, '_>,
        result: &mut XmlMetadataReferences,
    ) {
        let expressions = root
            .descendants()
            .filter(|node| {
                node.is_element()
                    && matches!(node.tag_name().name(), "calculatedField" | "expression")
            })
            .collect::<Vec<_>>();
        if expressions.is_empty() {
            return;
        }
        let mut handled = rustc_hash::FxHashSet::default();
        if let Some(dataset) = self.expression_dataset(root, result) {
            let fields = elements(dataset)
                .filter(|node| node.has_tag_name((DCS, "field")))
                .collect::<Vec<_>>();
            let calculated = elements(root)
                .filter(|node| node.has_tag_name((DCS, "calculatedField")))
                .collect::<Vec<_>>();
            let names = calculated
                .iter()
                .filter_map(|node| single(*node, "dataPath").and_then(|name| name.text()))
                .collect::<Vec<_>>();
            for calculation in calculated {
                let value = self.calculation_path(calculation);
                let Some((name, expression, path)) = value else { continue };
                // A calculated name must not shadow a dataset binding or another calculated field.
                if names
                    .iter()
                    .filter(|candidate| stdx::case::eq_ignore_case(candidate, &name))
                    .count()
                    != 1
                    || fields
                        .iter()
                        .filter_map(|field| single(*field, "dataPath").and_then(|path| path.text()))
                        .any(|field| stdx::case::eq_ignore_case(field, &name))
                {
                    continue;
                }
                handled.insert(calculation.id());
                handled.insert(expression.id());
                result.data_set_expressions.push(XmlDataSetExpression { query: 0, path });
                result.problems.push(XmlReferenceProblem::DataSetOutputContract);
            }
        }
        for node in expressions {
            if !handled.contains(&node.id()) {
                result.problems.push(XmlReferenceProblem::UnknownReferenceContract);
            }
        }
    }

    fn expression_dataset<'a, 'input>(
        &self,
        root: roxmltree::Node<'a, 'input>,
        result: &XmlMetadataReferences,
    ) -> Option<roxmltree::Node<'a, 'input>> {
        if result.queries.len() != 1 {
            return None;
        }
        let mut datasets = elements(root).filter(|node| node.tag_name().name() == "dataSet");
        let dataset = datasets.next()?;
        if datasets.next().is_some()
            || !dataset.has_tag_name((DCS, "dataSet"))
            || dataset.attributes().len() != 1
            || !typed(dataset, "DataSetQuery")
        {
            return None;
        }
        if self.value(single(dataset, "query")?)?.text != result.queries[0].text {
            return None;
        }
        let data_source = single(dataset, "dataSource")?.text()?;
        let sources = elements(root)
            .filter(|node| node.has_tag_name((DCS, "dataSource")))
            .filter(|node| single(*node, "name").and_then(|name| name.text()) == Some(data_source))
            .collect::<Vec<_>>();
        let [source] = sources.as_slice() else { return None };
        if single(*source, "dataSourceType")?.text() != Some("Local") {
            return None;
        }
        if !elements(dataset).filter(|node| node.tag_name().name() == "field").all(field_shape) {
            return None;
        }
        Some(dataset)
    }

    fn calculation_path<'a, 'input>(
        &self,
        node: roxmltree::Node<'a, 'input>,
    ) -> Option<(String, roxmltree::Node<'a, 'input>, XmlQueryText)> {
        if node.attributes().len() != 0 {
            return None;
        }
        let mut names = std::collections::BTreeSet::new();
        if !elements(node).all(|child| {
            child.tag_name().namespace() == Some(DCS)
                && names.insert(child.tag_name().name())
                && match child.tag_name().name() {
                    "dataPath" | "expression" | "title" => leaf(child),
                    "valueType" => {
                        leaf(child) && child.text().is_none_or(|text| text.trim().is_empty())
                    }
                    _ => false,
                }
        }) {
            return None;
        }
        let name = self.value(single(node, "dataPath")?)?.text;
        if !properties::identifier(&name) {
            return None;
        }
        let expression = single(node, "expression")?;
        let path = self.value(expression)?;
        let tokens = path.tokens()?;
        if tokens.len() < 2 || !tokens.iter().all(|token| properties::identifier(&token.text)) {
            return None;
        }
        Some((name, expression, path))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const XML: &str = include_str!("../../fixtures/rename/composition-calculated-path.xml");

    fn inspect(xml: &str) -> XmlMetadataReferences {
        MetadataXmlSource::decode(xml.as_bytes()).unwrap().inspect(None)
    }

    #[test]
    fn direct_calculated_field_paths_have_raw_spans_and_require_output_binding() {
        let xml = format!("\u{feff}{}", XML.replace('\n', "\r\n"));
        let facts = inspect(&xml);
        assert!(
            facts
                .problems
                .iter()
                .all(|problem| *problem == XmlReferenceProblem::DataSetOutputContract),
            "{facts:#?}"
        );
        assert_eq!(facts.data_set_expressions.len(), 2);
        let tokens = facts.data_set_expressions[0].path.tokens().unwrap();
        assert_eq!(
            tokens.iter().map(|token| token.text.as_str()).collect::<Vec<_>>(),
            ["Partner", "Caption"]
        );
        assert_eq!(&xml.as_bytes()[tokens[1].range.clone()], b"Cap&#116;ion");
    }

    #[test]
    fn general_expressions_foreign_namespaces_and_ambiguous_dataset_shapes_stay_incomplete() {
        for xml in [
            XML.replacen("Partner.Cap&#116;ion", "Sum(Partner.Caption)", 1),
            XML.replacen("Partner.Cap&#116;ion", "Partner.Caption + 1", 1),
            XML.replacen("Partner.Cap&#116;ion", "&amp;Dynamic", 1),
            XML.replacen("<expression>", "<expression xmlns='urn:foreign'>", 1),
            XML.replacen("xsi:type=\"DataSetQuery\"", "xsi:type=\"DataSetObject\"", 1),
            XML.replacen(
                "<dataSourceType>Local</dataSourceType>",
                "<dataSourceType>External</dataSourceType>",
                1,
            ),
            XML.replacen("<dataPath>Caption</dataPath>", "<dataPath>Partner</dataPath>", 1),
            XML.replacen(
                "</DataCompositionSchema>",
                "<dataSet xsi:type='DataSetQuery'/></DataCompositionSchema>",
                1,
            ),
        ] {
            assert!(
                inspect(&xml).problems.contains(&XmlReferenceProblem::UnknownReferenceContract),
                "{xml}"
            );
        }
    }
}
