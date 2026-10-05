//! Tests the embedding contract independently of any host's filesystem layout.

use ruff_db::files::{File, system_path_to_file};
use ruff_db::system::{DbWithWritableSystem, SystemPath};
use ruff_python_ast::{AtomicNodeIndex, Identifier, StmtImportFrom};
use salsa::Setter;

use crate::db::tests::TestDb;
use crate::{
    Db, ImportingFile, Module, ModuleKind, ModuleName, ModuleResolution, ModuleResolveMode,
    ResolverFile, file_to_module, resolve_import, resolve_import_member, resolve_module,
    resolve_submodule,
};

#[salsa::input]
pub(crate) struct Overrides {
    #[returns(ref)]
    imports: Vec<Import>,
    #[returns(ref)]
    pub(crate) modules: Vec<Entry>,
    #[returns(ref)]
    children: Vec<Child>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Import {
    referrer: File,
    name: String,
    level: u32,
    target: Option<File>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Entry {
    pub(crate) file: File,
    name: ModuleName,
    kind: ModuleKind,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct Child {
    parent: File,
    name: String,
    target: Option<File>,
}

pub(crate) fn resolve<'db>(
    db: &'db TestDb,
    importing_file: ImportingFile<'db>,
    name: Option<&ModuleName>,
    level: u32,
) -> ModuleResolution<'db> {
    let Some(overrides) = db.overrides else {
        return ModuleResolution::Fallback;
    };
    let name = name.map_or("", ModuleName::as_str);
    let Some(import) = overrides.imports(db).iter().find(|import| {
        import.referrer == importing_file.file(db) && import.name == name && import.level == level
    }) else {
        return ModuleResolution::Fallback;
    };
    result(db, importing_file.resolver_file(db), import.target)
}

pub(crate) fn child<'db>(
    db: &'db TestDb,
    importing_file: ImportingFile<'db>,
    parent: Module<'db>,
    name: &ModuleName,
) -> ModuleResolution<'db> {
    let Some(overrides) = db.overrides else {
        return ModuleResolution::Fallback;
    };
    let Some(child) = overrides
        .children(db)
        .iter()
        .find(|child| Some(child.parent) == parent.file(db) && child.name == name.as_str())
    else {
        return ModuleResolution::Fallback;
    };
    result(db, importing_file.resolver_file(db), child.target)
}

pub(crate) fn module<'db>(db: &'db TestDb, file: ResolverFile<'db>) -> ModuleResolution<'db> {
    result(db, file, Some(file.file(db)))
}

fn result<'db>(
    db: &'db TestDb,
    context: ResolverFile<'db>,
    file: Option<File>,
) -> ModuleResolution<'db> {
    let Some(file) = file else {
        return ModuleResolution::NotFound;
    };
    let Some(overrides) = db.overrides else {
        return ModuleResolution::Fallback;
    };
    let Some(entry) = overrides
        .modules(db)
        .iter()
        .find(|entry| entry.file == file)
    else {
        return ModuleResolution::Fallback;
    };
    ModuleResolution::Resolved(Module::new(
        db,
        file,
        context.environment(db),
        entry.name.clone(),
        entry.kind,
    ))
}

fn file(db: &mut TestDb, path: &str) -> File {
    db.write_file(path, "value = 1\n").unwrap();
    system_path_to_file(db, SystemPath::new(path)).unwrap()
}

fn entry(file: File, name: &str, kind: ModuleKind) -> Entry {
    Entry {
        file,
        name: ModuleName::new(name).unwrap(),
        kind,
    }
}

fn import(referrer: File, name: &str, level: u32, target: Option<File>) -> Import {
    Import {
        referrer,
        name: name.into(),
        level,
        target,
    }
}

#[test]
fn referrers_aliases_relative_levels_and_reverse_lookup() {
    let mut db = TestDb::new();
    let left = file(&mut db, "/left.py");
    let right = file(&mut db, "/right.py");
    let first = file(&mut db, "/generated/one.source");
    let second = file(&mut db, "/generated/two.source");
    db.overrides = Some(Overrides::new(
        &db,
        vec![
            import(left, "dependency", 0, Some(first)),
            import(right, "dependency", 0, Some(second)),
            import(left, "alias", 0, Some(first)),
            import(left, "dependency", 1, Some(second)),
        ],
        vec![
            entry(first, "canonical", ModuleKind::Module),
            entry(second, "canonical", ModuleKind::Module),
        ],
        vec![],
    ));
    let environment = db.resolver_environment();
    let left = ImportingFile::File(left, environment);
    let right = ImportingFile::File(right, environment);
    let name = ModuleName::new("dependency").unwrap();
    let resolved = resolve_module(&db, left, &name).unwrap();
    assert_eq!(resolved.file(&db), Some(first));
    assert_eq!(
        resolve_module(&db, right, &name).unwrap().file(&db),
        Some(second)
    );
    assert_eq!(
        resolve_module(&db, left, &ModuleName::new("alias").unwrap()),
        Some(resolved)
    );
    assert_eq!(
        resolve_import(&db, left, Some(&name), 1, ModuleResolveMode::Typing)
            .unwrap()
            .unwrap()
            .file(&db),
        Some(second)
    );
    assert_eq!(
        file_to_module(&db, ResolverFile::new(&db, first, environment)),
        Some(resolved)
    );
}

#[test]
fn submodules_use_parent_identity_instead_of_name() {
    let mut db = TestDb::new();
    let importer = file(&mut db, "/main.py");
    let first = file(&mut db, "/one/package.source");
    let second = file(&mut db, "/two/package.source");
    let first_child = file(&mut db, "/one/child.source");
    let second_child = file(&mut db, "/two/child.source");
    db.overrides = Some(Overrides::new(
        &db,
        vec![],
        vec![
            entry(first, "package", ModuleKind::Package),
            entry(second, "package", ModuleKind::Package),
            entry(first_child, "package.child", ModuleKind::Module),
            entry(second_child, "package.child", ModuleKind::Module),
        ],
        vec![
            Child {
                parent: first,
                name: "child".into(),
                target: Some(first_child),
            },
            Child {
                parent: second,
                name: "child".into(),
                target: Some(second_child),
            },
        ],
    ));
    let environment = db.resolver_environment();
    let importer = ImportingFile::File(importer, environment);
    for (parent, expected) in [(first, first_child), (second, second_child)] {
        let parent = file_to_module(&db, ResolverFile::new(&db, parent, environment)).unwrap();
        let child = resolve_submodule(
            &db,
            importer,
            parent,
            &ModuleName::new("child").unwrap(),
            ModuleResolveMode::Typing,
        )
        .unwrap();
        assert_eq!(child.file(&db), Some(expected));
    }
}

#[salsa::tracked]
fn dependency<'db>(db: &'db dyn Db, file: ResolverFile<'db>) -> Option<Module<'db>> {
    resolve_module(
        db,
        ImportingFile::ResolverFile(file),
        &ModuleName::new("dependency").unwrap(),
    )
}

#[test]
fn tracked_mapping_changes_invalidate_failed_and_successful_lookups() {
    let mut db = crate::testing::TestCaseBuilder::new()
        .with_vendored_typeshed()
        .with_src_files(&[("dependency.py", "value = 1")])
        .build()
        .db;
    let importer = file(&mut db, "/src/main.py");
    let target = file(&mut db, "/generated.py");
    // Prove that NotFound must suppress a module the standard resolver can find.
    assert!(
        resolve_module(
            &db,
            ImportingFile::File(importer, db.resolver_environment()),
            &ModuleName::new("dependency").unwrap()
        )
        .is_some()
    );
    let overrides = Overrides::new(
        &db,
        vec![import(importer, "dependency", 0, None)],
        vec![entry(target, "canonical", ModuleKind::Module)],
        vec![],
    );
    db.overrides = Some(overrides);
    let file = ResolverFile::new(&db, importer, db.resolver_environment());
    assert!(dependency(&db, file).is_none());
    overrides
        .set_imports(&mut db)
        .to(vec![import(importer, "dependency", 0, Some(target))]);
    let file = ResolverFile::new(&db, importer, db.resolver_environment());
    assert_eq!(dependency(&db, file).unwrap().file(&db), Some(target));
    overrides
        .set_imports(&mut db)
        .to(vec![import(importer, "dependency", 0, None)]);
    let file = ResolverFile::new(&db, importer, db.resolver_environment());
    assert!(dependency(&db, file).is_none());
    // Declining the override restores the standard-library resolver.
    assert!(
        resolve_module(
            &db,
            ImportingFile::ResolverFile(file),
            &ModuleName::new("math").unwrap()
        )
        .is_some()
    );
}

#[test]
fn from_import_members_preserve_the_original_context() {
    let mut db = TestDb::new();
    let importer = file(&mut db, "/main.py");
    let package = file(&mut db, "/package.source");
    let relative = file(&mut db, "/relative.source");
    let absolute = file(&mut db, "/absolute.source");
    db.overrides = Some(Overrides::new(
        &db,
        vec![
            import(importer, "child", 1, Some(relative)),
            import(importer, "alias.child", 0, Some(absolute)),
            import(importer, "child", 2, None),
        ],
        vec![
            entry(package, "canonical", ModuleKind::Package),
            entry(relative, "child", ModuleKind::Module),
            entry(absolute, "child", ModuleKind::Module),
        ],
        vec![],
    ));
    let environment = db.resolver_environment();
    let importer = ImportingFile::File(importer, environment);
    let parent = file_to_module(&db, ResolverFile::new(&db, package, environment)).unwrap();
    for (module, level, expected) in [
        (None, 1, Some(relative)),
        (Some("alias"), 0, Some(absolute)),
        (None, 2, None),
    ] {
        let range = (0.into()..0.into()).into();
        let statement = StmtImportFrom {
            node_index: AtomicNodeIndex::NONE,
            range,
            module: module.map(|name| Identifier::new(name, range)),
            names: vec![],
            level,
            is_lazy: false,
        };
        let result = resolve_import_member(
            &db,
            importer,
            parent,
            &statement,
            "child",
            None,
            ModuleResolveMode::Typing,
        );
        match (result, expected) {
            (ModuleResolution::Resolved(module), Some(file)) => {
                assert_eq!(module.file(&db), Some(file));
            }
            (ModuleResolution::NotFound, None) => {}
            _ => panic!("the host did not receive the original import context"),
        }
    }
}

#[test]
fn explicit_intrinsic_names_consult_the_host() {
    let mut db = TestDb::new();
    let importer = file(&mut db, "/main.py");
    let target = file(&mut db, "/generated/dependency.source");
    let names = ["sys", "types", "builtins", "typing_extensions"];
    db.overrides = Some(Overrides::new(
        &db,
        names
            .iter()
            .map(|name| import(importer, name, 0, Some(target)))
            .collect(),
        vec![entry(target, "canonical", ModuleKind::Module)],
        vec![],
    ));
    let environment = db.resolver_environment();
    let importer = ImportingFile::File(importer, environment);
    for name in names {
        let name = ModuleName::new(name).unwrap();
        let module = resolve_module(&db, importer, &name).unwrap();
        assert_eq!(module.file(&db), Some(target));
        assert_eq!(module.name(&db).as_str(), "canonical");
        assert_eq!(module.display_name(&db), "/generated/dependency.source");
        // Intrinsic lookups have no importing file and retain the standard resolution path.
        assert_ne!(
            crate::resolve_module_confident(&db, environment, &name),
            Some(module)
        );
    }
}
