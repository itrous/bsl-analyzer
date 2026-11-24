use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct CfeFixtureBuilder {
    base_config_xml: String,
    base_modules: Vec<CfeModuleSpec>,
    extensions: Vec<CfeExtensionSpec>,
}

#[derive(Debug)]
struct CfeExtensionSpec {
    name: String,
    config_xml: String,
    modules: Vec<CfeModuleSpec>,
}

#[derive(Debug)]
struct CfeModuleSpec {
    name: String,
    source: String,
    global: bool,
}

#[derive(Debug)]
pub struct CfeFixture {
    root: PathBuf,
    base_modules: Vec<CfeModule>,
    extensions: Vec<CfeExtension>,
}

#[derive(Debug)]
pub struct CfeExtension {
    name: String,
    root: PathBuf,
    config_xml: String,
    modules: Vec<CfeModule>,
}

#[derive(Debug)]
pub struct CfeModule {
    name: String,
    path: PathBuf,
    source: String,
    global: bool,
}

impl CfeFixtureBuilder {
    pub fn new(base_config_xml: &str) -> Self {
        Self {
            base_config_xml: base_config_xml.to_string(),
            base_modules: Vec::new(),
            extensions: Vec::new(),
        }
    }

    /// A common module of the BASE configuration. Needed whenever a fixture must
    /// exercise a module that has more than one body — the base declaration and an
    /// extension's adoption of the same name.
    pub fn add_base_module(&mut self, module_name: &str, bsl: &str) -> &mut Self {
        self.add_base_module_with_global(module_name, bsl, false)
    }

    /// A base common module declared GLOBAL, so its exports enter the global context and
    /// a bare `Имя()` call can resolve to them.
    pub fn add_base_module_global(&mut self, module_name: &str, bsl: &str) -> &mut Self {
        self.add_base_module_with_global(module_name, bsl, true)
    }

    fn add_base_module_with_global(
        &mut self,
        module_name: &str,
        bsl: &str,
        global: bool,
    ) -> &mut Self {
        if let Some(existing) = self.base_modules.iter_mut().find(|m| m.name == module_name) {
            existing.source = bsl.to_string();
            existing.global = global;
        } else {
            self.base_modules.push(CfeModuleSpec {
                name: module_name.to_string(),
                source: bsl.to_string(),
                global,
            });
        }
        self
    }

    pub fn add_extension(&mut self, name: &str, config_xml: &str) -> &mut Self {
        if let Some(existing) = self.extensions.iter_mut().find(|ext| ext.name == name) {
            existing.config_xml = config_xml.to_string();
            return self;
        }

        self.extensions.push(CfeExtensionSpec {
            name: name.to_string(),
            config_xml: config_xml.to_string(),
            modules: Vec::new(),
        });
        self
    }

    pub fn add_extension_module(
        &mut self,
        ext_name: &str,
        module_name: &str,
        bsl: &str,
    ) -> &mut Self {
        let ext = match self.extensions.iter_mut().find(|ext| ext.name == ext_name) {
            Some(ext) => ext,
            None => {
                self.extensions.push(CfeExtensionSpec {
                    name: ext_name.to_string(),
                    config_xml: String::new(),
                    modules: Vec::new(),
                });
                self.extensions.last_mut().expect("extension was just pushed")
            }
        };

        if let Some(existing) = ext.modules.iter_mut().find(|module| module.name == module_name) {
            existing.source = bsl.to_string();
        } else {
            ext.modules.push(CfeModuleSpec {
                name: module_name.to_string(),
                source: bsl.to_string(),
                global: false,
            });
        }
        self
    }

    pub fn build(self) -> CfeFixture {
        let root = next_cfe_fixture_root();
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).expect("create CFE fixture root");

        let base_config_xml = if self.base_config_xml.trim().is_empty() {
            minimal_configuration_xml("TestConfiguration")
        } else {
            self.base_config_xml
        };
        std::fs::write(root.join("Configuration.xml"), base_config_xml)
            .expect("write base Configuration.xml");

        let base_modules = self
            .base_modules
            .into_iter()
            .map(|module| {
                let module_dir = root.join("CommonModules").join(&module.name);
                std::fs::create_dir_all(&module_dir).expect("create base CommonModule directory");
                let path = module_dir.join("Module.bsl");
                std::fs::write(&path, &module.source).expect("write base CommonModule body");
                CfeModule { name: module.name, path, source: module.source, global: module.global }
            })
            .collect();

        let extensions_root = root.join("Extensions");
        std::fs::create_dir_all(&extensions_root).expect("create Extensions directory");

        let extensions = self
            .extensions
            .into_iter()
            .map(|ext| {
                let ext_root = extensions_root.join(&ext.name);
                std::fs::create_dir_all(&ext_root).expect("create CFE extension directory");

                let config_xml = if ext.config_xml.trim().is_empty() {
                    minimal_configuration_xml(&ext.name)
                } else {
                    ext.config_xml
                };
                std::fs::write(ext_root.join("Configuration.xml"), &config_xml)
                    .expect("write extension Configuration.xml");

                let modules = ext
                    .modules
                    .into_iter()
                    .map(|module| {
                        let module_dir = ext_root.join("CommonModules").join(&module.name);
                        std::fs::create_dir_all(&module_dir)
                            .expect("create CFE CommonModule directory");
                        let path = module_dir.join("Module.bsl");
                        std::fs::write(&path, &module.source).expect("write CFE CommonModule body");

                        CfeModule {
                            name: module.name,
                            path,
                            source: module.source,
                            global: module.global,
                        }
                    })
                    .collect();

                CfeExtension { name: ext.name, root: ext_root, config_xml, modules }
            })
            .collect();

        CfeFixture { root, base_modules, extensions }
    }
}

impl CfeFixture {
    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn base_modules(&self) -> &[CfeModule] {
        &self.base_modules
    }

    pub fn extensions(&self) -> &[CfeExtension] {
        &self.extensions
    }

    pub fn config_paths(&self) -> Vec<(Option<String>, PathBuf)> {
        let mut paths = Vec::with_capacity(self.extensions.len() + 1);
        paths.push((None, self.root.clone()));
        paths.extend(self.extensions.iter().map(|ext| (Some(ext.name.clone()), ext.root.clone())));
        paths
    }
}

impl Drop for CfeFixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl CfeExtension {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn config_xml(&self) -> &str {
        &self.config_xml
    }

    pub fn modules(&self) -> &[CfeModule] {
        &self.modules
    }
}

impl CfeModule {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Whether the module is declared global — its exports enter the global context.
    pub fn is_global(&self) -> bool {
        self.global
    }
}

fn next_cfe_fixture_root() -> PathBuf {
    stdx::test_temp::unique_dir("bsl_analyzer_cfe")
}

fn minimal_configuration_xml(name: &str) -> String {
    format!(
        r#"<MetaDataObject xmlns="http://v8.1c.ru/8.3/MDClasses" version="2.10">
    <Configuration uuid="00000000-0000-0000-0000-000000000000">
        <Properties>
            <Name>{}</Name>
        </Properties>
    </Configuration>
</MetaDataObject>"#,
        escape_xml_text(name)
    )
}

fn escape_xml_text(input: &str) -> String {
    input
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_writes_real_cfe_layout() {
        let mut builder = CfeFixtureBuilder::new("");
        builder.add_extension("ExtA", "").add_extension_module(
            "ExtA",
            "ApiModule",
            "Процедура Метод() Экспорт\nКонецПроцедуры",
        );

        let fixture = builder.build();

        assert!(fixture.root().join("Configuration.xml").is_file());
        assert!(fixture
            .root()
            .join("Extensions/ExtA/CommonModules/ApiModule/Module.bsl")
            .is_file());
        assert_eq!(fixture.config_paths().len(), 2);
    }

    #[test]
    fn drop_removes_temp_root() {
        let root = {
            let fixture = CfeFixtureBuilder::new("").build();
            let root = fixture.root().to_path_buf();
            assert!(root.exists());
            root
        };

        assert!(!root.exists());
    }
}
