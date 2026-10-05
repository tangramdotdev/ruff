use std::borrow::Cow;

use ruff_db::Db as SourceDb;
use ruff_python_ast::StmtImportFrom;

use crate::{ImportingFile, Module, ModuleName, ModuleResolveMode, ResolverFile};

#[salsa::db]
pub trait Db: SourceDb {
    /// Resolves an import in its original file context. `level` is zero for absolute imports.
    ///
    /// Overrides must read tracked inputs or data immutable for the database lifetime. Returning
    /// `NotFound` suppresses filesystem fallback. Implementations must enforce Python's relative
    /// import bounds using the package context they provide through `file_to_module`.
    fn resolve_module<'db>(
        &'db self,
        _importing_file: ImportingFile<'db>,
        _name: Option<&ModuleName>,
        _level: u32,
        _mode: ModuleResolveMode,
    ) -> ModuleResolution<'db> {
        ModuleResolution::Fallback
    }

    /// Resolves a child of an already resolved package, independently of its import alias.
    /// The importing file is retained for hosts whose dependency graph is referrer-dependent.
    fn resolve_submodule<'db>(
        &'db self,
        _importing_file: ImportingFile<'db>,
        _parent: Module<'db>,
        _name: &ModuleName,
        _mode: ModuleResolveMode,
    ) -> ModuleResolution<'db> {
        ModuleResolution::Fallback
    }

    /// Optionally resolves a from-import member, including an existing module-valued export.
    ///
    /// `export` is the module already bound to the member, or `None` when no export exists.
    /// Non-module exports retain Python's normal attribute precedence. `Fallback` preserves an
    /// existing export or continues with ordinary child lookup; `NotFound` rejects the import.
    fn resolve_import_member<'db>(
        &'db self,
        _importing_file: ImportingFile<'db>,
        _parent: Module<'db>,
        _import: &StmtImportFrom,
        _member: &str,
        _export: Option<Module<'db>>,
        _mode: ModuleResolveMode,
    ) -> ModuleResolution<'db> {
        ModuleResolution::Fallback
    }

    /// Optionally supplies a diagnostic label without changing a module's canonical identity.
    /// Like resolution overrides, labels must come from tracked inputs or immutable data.
    fn module_display_name<'db>(&'db self, _module: Module<'db>) -> Option<Cow<'db, str>> {
        None
    }

    /// Returns a file's canonical module identity and package context.
    fn file_to_module<'db>(&'db self, _file: ResolverFile<'db>) -> ModuleResolution<'db> {
        ModuleResolution::Fallback
    }
}

/// The result of an optional host resolution override.
#[derive(Clone, Copy, Debug, Eq, PartialEq, get_size2::GetSize, salsa::SalsaValue)]
pub enum ModuleResolution<'db> {
    /// Continue with the standard resolver.
    Fallback,
    /// Use this canonical module.
    Resolved(Module<'db>),
    /// The import is unresolved; do not search other locations.
    NotFound,
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::{Arc, Mutex};

    use ruff_db::Db as SourceDb;
    use ruff_db::files::Files;
    use ruff_db::system::{DbWithTestSystem, TestSystem};
    use ruff_db::vendored::VendoredFileSystem;
    use ruff_python_ast::{PythonVersion, StmtImportFrom};

    use super::Db;
    use crate::{ResolverEnvironment, resolve::SearchPaths};

    type Events = Arc<Mutex<Vec<salsa::Event>>>;

    #[salsa::db]
    #[derive(Clone)]
    pub(crate) struct TestDb {
        storage: salsa::Storage<Self>,
        pub(crate) overrides: Option<crate::custom::Overrides>,
        files: Files,
        system: TestSystem,
        vendored: VendoredFileSystem,
        search_paths: Arc<SearchPaths>,
        python_version: PythonVersion,
        events: Events,
    }

    impl TestDb {
        pub(crate) fn new() -> Self {
            let events = Events::default();
            Self {
                storage: salsa::Storage::new(Some(Box::new({
                    let events = events.clone();
                    move |event| {
                        tracing::trace!("event: {event:?}");
                        let mut events = events.lock().unwrap();
                        events.push(event);
                    }
                }))),
                overrides: None,
                system: TestSystem::default(),
                vendored: ty_vendored::file_system().clone(),
                files: Files::default(),
                search_paths: Arc::new(SearchPaths::empty(ty_vendored::file_system())),
                python_version: PythonVersion::default(),
                events,
            }
        }

        pub(crate) fn with_search_paths(mut self, search_paths: SearchPaths) -> Self {
            self.set_search_paths(search_paths);
            self
        }

        pub(crate) fn with_python_version(mut self, python_version: PythonVersion) -> Self {
            self.python_version = python_version;
            self
        }

        pub(crate) fn set_search_paths(&mut self, search_paths: SearchPaths) {
            search_paths.try_register_static_roots(self);
            self.search_paths = Arc::new(search_paths);
        }

        pub(crate) fn search_paths(&self) -> &SearchPaths {
            &self.search_paths
        }

        pub(crate) fn resolver_environment(&self) -> ResolverEnvironment<'_> {
            ResolverEnvironment::new(self, self.python_version, self.search_paths.as_ref())
        }

        /// Takes the salsa events.
        pub(crate) fn take_salsa_events(&mut self) -> Vec<salsa::Event> {
            let mut events = self.events.lock().unwrap();
            std::mem::take(&mut *events)
        }

        /// Clears the salsa events.
        pub(crate) fn clear_salsa_events(&mut self) {
            self.take_salsa_events();
        }
    }

    impl DbWithTestSystem for TestDb {
        fn test_system(&self) -> &TestSystem {
            &self.system
        }

        fn test_system_mut(&mut self) -> &mut TestSystem {
            &mut self.system
        }
    }

    #[salsa::db]
    impl SourceDb for TestDb {
        fn vendored(&self) -> &VendoredFileSystem {
            &self.vendored
        }

        fn system(&self) -> &dyn ruff_db::system::System {
            &self.system
        }

        fn files(&self) -> &Files {
            &self.files
        }
    }

    #[salsa::db]
    impl Db for TestDb {
        fn resolve_module<'db>(
            &'db self,
            file: crate::ImportingFile<'db>,
            name: Option<&crate::ModuleName>,
            level: u32,
            _mode: crate::ModuleResolveMode,
        ) -> super::ModuleResolution<'db> {
            crate::custom::resolve(self, file, name, level)
        }
        fn resolve_submodule<'db>(
            &'db self,
            file: crate::ImportingFile<'db>,
            parent: crate::Module<'db>,
            name: &crate::ModuleName,
            _mode: crate::ModuleResolveMode,
        ) -> super::ModuleResolution<'db> {
            crate::custom::child(self, file, parent, name)
        }
        fn resolve_import_member<'db>(
            &'db self,
            file: crate::ImportingFile<'db>,
            _parent: crate::Module<'db>,
            import: &StmtImportFrom,
            member: &str,
            _export: Option<crate::Module<'db>>,
            _mode: crate::ModuleResolveMode,
        ) -> super::ModuleResolution<'db> {
            let name = match import.module.as_deref() {
                Some(module) => format!("{module}.{member}"),
                None => member.to_owned(),
            };
            crate::custom::resolve(
                self,
                file,
                crate::ModuleName::new(&name).as_ref(),
                import.level,
            )
        }
        fn module_display_name<'db>(
            &'db self,
            module: crate::Module<'db>,
        ) -> Option<std::borrow::Cow<'db, str>> {
            let file = module.file(self)?;
            let overrides = self.overrides?;
            overrides
                .modules(self)
                .iter()
                .find(|entry| entry.file == file)?;
            Some(std::borrow::Cow::Owned(file.path(self).to_string()))
        }
        fn file_to_module<'db>(
            &'db self,
            file: crate::ResolverFile<'db>,
        ) -> super::ModuleResolution<'db> {
            crate::custom::module(self, file)
        }
    }

    #[salsa::db]
    impl salsa::Database for TestDb {}
}
