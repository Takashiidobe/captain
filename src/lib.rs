use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug)]
pub enum Error {
    Io { path: PathBuf, source: io::Error },
    CapnpFailed { command: String, stderr: String },
    CommandFailed { command: String, stderr: String },
    NoSchemas(PathBuf),
    Usage(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Error::Io { path, source } => write!(f, "{}: {}", path.display(), source),
            Error::CapnpFailed { command, stderr } => {
                write!(f, "`{command}` failed")?;
                if !stderr.trim().is_empty() {
                    write!(f, "\n{}", stderr.trim())?;
                }
                Ok(())
            }
            Error::CommandFailed { command, stderr } => {
                write!(f, "`{command}` failed")?;
                if !stderr.trim().is_empty() {
                    write!(f, "\n{}", stderr.trim())?;
                }
                Ok(())
            }
            Error::NoSchemas(path) => write!(f, "no .capnp files found under {}", path.display()),
            Error::Usage(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckConfig {
    pub before: Vec<String>,
    pub after: Vec<String>,
    pub before_ref: Option<String>,
    pub after_ref: Option<String>,
    pub compare_ref: Option<String>,
    pub paths: Vec<String>,
    pub import_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ResolvedCheck {
    before: Vec<String>,
    after: Vec<String>,
    before_import_paths: Vec<PathBuf>,
    after_import_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub violations: Vec<Violation>,
    pub lints: Vec<Lint>,
}

impl Report {
    pub fn is_compatible(&self) -> bool {
        self.violations.is_empty() && !self.has_error_lints()
    }

    fn has_error_lints(&self) -> bool {
        self.lints
            .iter()
            .any(|lint| lint.severity == LintSeverity::Error)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub reason: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Lint {
    pub severity: LintSeverity,
    pub path: String,
    pub reason: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LintSeverity {
    Error,
    Warning,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub nodes: BTreeMap<String, Node>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SchemaSet {
    pub snapshot: Snapshot,
    pub files: Vec<SchemaFile>,
    pub node_paths: BTreeMap<String, String>,
    pub lints: Vec<Lint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaFile {
    pub path: PathBuf,
    pub relative_path: String,
    pub file_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Node {
    pub kind: NodeKind,
    pub fields: BTreeMap<u32, Field>,
    pub enum_values: BTreeMap<u32, EnumValue>,
    pub methods: BTreeMap<u32, Method>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Struct,
    Enum,
    Interface,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub ty: String,
    pub default: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumValue {
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Method {
    pub name: String,
    pub signature: String,
}

pub fn check(config: &CheckConfig) -> Result<Report, Error> {
    match check_mode(config)? {
        CheckMode::Filesystem(resolved) => check_resolved(&resolved),
        CheckMode::GitRefs {
            before_ref,
            after_ref,
            paths,
            import_paths,
        } => {
            let export = GitExport::new(&before_ref, &after_ref, &paths, &import_paths)?;
            let resolved = ResolvedCheck {
                before: prefix_sources(&export.before_dir, &paths),
                after: prefix_sources(&export.after_dir, &paths),
                before_import_paths: prefix_import_paths(&export.before_dir, &import_paths),
                after_import_paths: prefix_import_paths(&export.after_dir, &import_paths),
            };
            check_resolved(&resolved)
        }
        CheckMode::CompareRef {
            compare_ref,
            paths,
            import_paths,
        } => {
            let export = WorktreeExport::new(&compare_ref, &paths, &import_paths)?;
            let resolved = ResolvedCheck {
                before: prefix_sources(&export.before_dir, &paths),
                after: prefix_sources(&export.after_dir, &paths),
                before_import_paths: prefix_import_paths(&export.before_dir, &import_paths),
                after_import_paths: prefix_import_paths(&export.after_dir, &import_paths),
            };
            check_resolved(&resolved)
        }
    }
}

enum CheckMode {
    Filesystem(ResolvedCheck),
    GitRefs {
        before_ref: String,
        after_ref: String,
        paths: Vec<String>,
        import_paths: Vec<PathBuf>,
    },
    CompareRef {
        compare_ref: String,
        paths: Vec<String>,
        import_paths: Vec<PathBuf>,
    },
}

fn check_resolved(config: &ResolvedCheck) -> Result<Report, Error> {
    let before_sources = discover_capnp_sources(&config.before)?;
    let after_sources = discover_capnp_sources(&config.after)?;
    let before = snapshot_sources(&before_sources)?;
    let after = snapshot_sources(&after_sources)?;
    let report = compare(&before, &after);

    if report.has_error_lints() {
        return Ok(report);
    }

    compile_with_capnp(
        &before_sources.files,
        &before_sources.import_roots,
        &config.before_import_paths,
    )?;
    compile_with_capnp(
        &after_sources.files,
        &after_sources.import_roots,
        &config.after_import_paths,
    )?;

    Ok(report)
}

fn check_mode(config: &CheckConfig) -> Result<CheckMode, Error> {
    let filesystem_mode = !config.before.is_empty() || !config.after.is_empty();
    let ref_to_ref_mode = config.before_ref.is_some() || config.after_ref.is_some();
    let compare_ref_mode = config.compare_ref.is_some();

    if filesystem_mode && (ref_to_ref_mode || compare_ref_mode || !config.paths.is_empty()) {
        return Err(Error::Usage(format!(
            "cannot mix --before/--after with ref comparison flags\n\n{}",
            usage()
        )));
    }

    if ref_to_ref_mode && compare_ref_mode {
        return Err(Error::Usage(format!(
            "cannot mix --compare-ref with --before-ref/--after-ref\n\n{}",
            usage()
        )));
    }

    if ref_to_ref_mode {
        let before_ref = config
            .before_ref
            .clone()
            .ok_or_else(|| Error::Usage(format!("missing --before-ref\n\n{}", usage())))?;
        let after_ref = config
            .after_ref
            .clone()
            .ok_or_else(|| Error::Usage(format!("missing --after-ref\n\n{}", usage())))?;
        if config.paths.is_empty() {
            return Err(Error::Usage(format!("missing --path\n\n{}", usage())));
        }
        return Ok(CheckMode::GitRefs {
            before_ref,
            after_ref,
            paths: config.paths.clone(),
            import_paths: config.import_paths.clone(),
        });
    }

    if compare_ref_mode {
        if config.paths.is_empty() {
            return Err(Error::Usage(format!("missing --path\n\n{}", usage())));
        }
        return Ok(CheckMode::CompareRef {
            compare_ref: config
                .compare_ref
                .clone()
                .expect("compare ref checked above"),
            paths: config.paths.clone(),
            import_paths: config.import_paths.clone(),
        });
    }

    if !config.paths.is_empty() {
        return Err(Error::Usage(format!(
            "--path requires --before-ref/--after-ref or --compare-ref\n\n{}",
            usage()
        )));
    }

    if config.before.is_empty() {
        return Err(Error::Usage(format!("missing --before\n\n{}", usage())));
    }
    if config.after.is_empty() {
        return Err(Error::Usage(format!("missing --after\n\n{}", usage())));
    }

    Ok(CheckMode::Filesystem(ResolvedCheck {
        before: config.before.clone(),
        after: config.after.clone(),
        before_import_paths: config.import_paths.clone(),
        after_import_paths: config.import_paths.clone(),
    }))
}

pub fn compare(before: &SchemaSet, after: &SchemaSet) -> Report {
    let mut violations = Vec::new();
    let mut lints = lint_schema_sets(before, after);

    for (node_name, before_node) in &before.snapshot.nodes {
        let Some(after_node) = after.snapshot.nodes.get(node_name) else {
            violations.push(Violation {
                path: node_name.clone(),
                reason: "node was removed".to_owned(),
                before: Some(format!("{:?}", before_node.kind)),
                after: None,
            });
            continue;
        };

        if before_node.kind != after_node.kind {
            violations.push(Violation {
                path: node_name.clone(),
                reason: "node kind changed".to_owned(),
                before: Some(format!("{:?}", before_node.kind)),
                after: Some(format!("{:?}", after_node.kind)),
            });
            continue;
        }

        compare_struct_fields(
            node_name,
            before_node,
            after_node,
            &mut violations,
            &mut lints,
        );
        lint_field_default_changes(node_name, before_node, after_node, &mut lints);
        compare_enum_values(
            node_name,
            before_node,
            after_node,
            &mut violations,
            &mut lints,
        );
        compare_methods(
            node_name,
            before_node,
            after_node,
            &mut violations,
            &mut lints,
        );
    }

    Report { violations, lints }
}

fn snapshot_sources(sources: &DiscoveredSources) -> Result<SchemaSet, Error> {
    let mut set = snapshot_files(&sources.files)?;
    let root = common_file_root(&sources.files);
    for file in &sources.duplicate_files {
        set.lints.push(Lint {
            severity: LintSeverity::Error,
            path: relative_schema_path(file, &root),
            reason: "file was selected more than once".to_owned(),
            detail: None,
        });
    }
    Ok(set)
}

pub fn snapshot_files(files: &[PathBuf]) -> Result<SchemaSet, Error> {
    let mut snapshot = Snapshot::default();
    let mut schema_files = Vec::new();
    let mut lints = Vec::new();
    let mut node_paths = BTreeMap::new();
    let root = common_file_root(files);

    for file in files {
        let source = fs::read_to_string(file).map_err(|source| Error::Io {
            path: file.clone(),
            source,
        })?;
        let relative_path = relative_schema_path(file, &root);
        let file_snapshot = parse_schema_with_lints(&source);
        for mut lint in file_snapshot.lints {
            lint.path = format!("{relative_path}:{}", lint.path);
            lints.push(lint);
        }
        let file_metadata = parse_file_metadata(&source);
        for mut lint in file_metadata.lints {
            lint.path = relative_path.clone();
            lints.push(lint);
        }
        schema_files.push(SchemaFile {
            path: file.clone(),
            relative_path: relative_path.clone(),
            file_id: file_metadata.file_id,
        });
        for (name, node) in file_snapshot.snapshot.nodes {
            if let Some(first_path) = node_paths.get(&name) {
                lints.push(Lint {
                    severity: LintSeverity::Error,
                    path: name,
                    reason: "node name is defined in multiple files".to_owned(),
                    detail: Some(format!("{} and {}", first_path, relative_path)),
                });
            } else {
                node_paths.insert(name.clone(), relative_path.clone());
                snapshot.nodes.insert(name, node);
            }
        }
    }

    Ok(SchemaSet {
        snapshot,
        files: schema_files,
        node_paths,
        lints,
    })
}

pub fn parse_schema(source: &str) -> Snapshot {
    parse_schema_with_lints(source).snapshot
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ParsedSchema {
    snapshot: Snapshot,
    lints: Vec<Lint>,
}

fn parse_schema_with_lints(source: &str) -> ParsedSchema {
    let mut snapshot = Snapshot::default();
    let mut lints = Vec::new();
    let mut stack: Vec<(String, NodeKind)> = Vec::new();

    for statement in statements(source) {
        if statement == "}" {
            stack.pop();
            continue;
        }

        if let Some((kind, name)) = parse_node_start(&statement) {
            let full_name = if let Some((parent, _)) = stack.last() {
                format!("{parent}.{name}")
            } else {
                name
            };
            let node = Node {
                kind,
                fields: BTreeMap::new(),
                enum_values: BTreeMap::new(),
                methods: BTreeMap::new(),
            };
            if snapshot.nodes.contains_key(&full_name) {
                lints.push(Lint {
                    severity: LintSeverity::Error,
                    path: full_name.clone(),
                    reason: "node name is defined multiple times".to_owned(),
                    detail: None,
                });
            } else {
                snapshot.nodes.insert(full_name.clone(), node);
            }
            stack.push((full_name, kind));
            continue;
        }

        let Some((node_name, kind)) = stack.last().cloned() else {
            continue;
        };
        let Some(node) = snapshot.nodes.get_mut(&node_name) else {
            continue;
        };

        match kind {
            NodeKind::Struct => {
                if let Some((ordinal, field)) = parse_field(&statement) {
                    if let Some(existing_ordinal) = field_name_ordinal(node, &field.name) {
                        lints.push(Lint {
                            severity: LintSeverity::Error,
                            path: format!("{node_name}.field[{ordinal}]"),
                            reason: "field name is defined multiple times".to_owned(),
                            detail: Some(format!(
                                "{} also used by field[{existing_ordinal}]",
                                field.name
                            )),
                        });
                    }
                    if let Some(existing) = node.fields.get(&ordinal) {
                        lints.push(Lint {
                            severity: LintSeverity::Error,
                            path: format!("{node_name}.field[{ordinal}]"),
                            reason: "field ordinal is defined multiple times".to_owned(),
                            detail: Some(format!("{} and {}", existing.name, field.name)),
                        });
                    } else {
                        node.fields.insert(ordinal, field);
                    }
                }
            }
            NodeKind::Enum => {
                if let Some((ordinal, value)) = parse_enum_value(&statement) {
                    if let Some(existing_ordinal) = enum_value_name_ordinal(node, &value.name) {
                        lints.push(Lint {
                            severity: LintSeverity::Error,
                            path: format!("{node_name}.enum[{ordinal}]"),
                            reason: "enum value name is defined multiple times".to_owned(),
                            detail: Some(format!(
                                "{} also used by enum[{existing_ordinal}]",
                                value.name
                            )),
                        });
                    }
                    if let Some(existing) = node.enum_values.get(&ordinal) {
                        lints.push(Lint {
                            severity: LintSeverity::Error,
                            path: format!("{node_name}.enum[{ordinal}]"),
                            reason: "enum ordinal is defined multiple times".to_owned(),
                            detail: Some(format!("{} and {}", existing.name, value.name)),
                        });
                    } else {
                        node.enum_values.insert(ordinal, value);
                    }
                }
            }
            NodeKind::Interface => {
                if let Some((ordinal, method)) = parse_method(&statement) {
                    if let Some(existing_ordinal) = method_name_ordinal(node, &method.name) {
                        lints.push(Lint {
                            severity: LintSeverity::Error,
                            path: format!("{node_name}.method[{ordinal}]"),
                            reason: "method name is defined multiple times".to_owned(),
                            detail: Some(format!(
                                "{} also used by method[{existing_ordinal}]",
                                method.name
                            )),
                        });
                    }
                    if let Some(existing) = node.methods.get(&ordinal) {
                        lints.push(Lint {
                            severity: LintSeverity::Error,
                            path: format!("{node_name}.method[{ordinal}]"),
                            reason: "method ordinal is defined multiple times".to_owned(),
                            detail: Some(format!("{} and {}", existing.name, method.name)),
                        });
                    } else {
                        node.methods.insert(ordinal, method);
                    }
                }
            }
        }
    }

    ParsedSchema { snapshot, lints }
}

fn field_name_ordinal(node: &Node, name: &str) -> Option<u32> {
    node.fields
        .iter()
        .find_map(|(ordinal, field)| (field.name == name).then_some(*ordinal))
}

fn enum_value_name_ordinal(node: &Node, name: &str) -> Option<u32> {
    node.enum_values
        .iter()
        .find_map(|(ordinal, value)| (value.name == name).then_some(*ordinal))
}

fn method_name_ordinal(node: &Node, name: &str) -> Option<u32> {
    node.methods
        .iter()
        .find_map(|(ordinal, method)| (method.name == name).then_some(*ordinal))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSources {
    pub files: Vec<PathBuf>,
    pub import_roots: Vec<PathBuf>,
    pub duplicate_files: Vec<PathBuf>,
}

pub fn discover_capnp_sources(sources: &[String]) -> Result<DiscoveredSources, Error> {
    let mut files = Vec::new();
    let mut import_roots = BTreeSet::new();

    for source in sources {
        if has_glob_meta(source) {
            let matched = discover_glob(source, &mut files, &mut import_roots)?;
            if !matched {
                return Err(Error::NoSchemas(PathBuf::from(source)));
            }
        } else {
            let path = Path::new(source);
            discover_path(path, &mut files, &mut import_roots)?;
        }
    }

    files.sort();
    let duplicate_files = duplicate_paths(&files);
    files.dedup();

    if files.is_empty() {
        Err(Error::NoSchemas(PathBuf::from(sources.join(", "))))
    } else {
        let mut import_roots = import_roots.into_iter().collect::<Vec<_>>();
        import_roots.sort();
        Ok(DiscoveredSources {
            files,
            import_roots,
            duplicate_files,
        })
    }
}

pub fn discover_capnp_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    Ok(discover_capnp_sources(&[root.display().to_string()])?.files)
}

fn duplicate_paths(files: &[PathBuf]) -> Vec<PathBuf> {
    let mut duplicates = Vec::new();
    let mut previous = None;

    for file in files {
        if previous.is_some_and(|previous| previous == file) {
            duplicates.push(file.clone());
        }
        previous = Some(file);
    }

    duplicates
}

fn discover_path(
    path: &Path,
    files: &mut Vec<PathBuf>,
    import_roots: &mut BTreeSet<PathBuf>,
) -> Result<(), Error> {
    let metadata = fs::metadata(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;

    if metadata.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension == "capnp")
        {
            if let Some(parent) = path.parent() {
                import_roots.insert(parent.to_owned());
            }
            files.push(path.to_owned());
        }
        return Ok(());
    }

    import_roots.insert(path.to_owned());
    discover_rec(path, files)
}

fn discover_rec(path: &Path, files: &mut Vec<PathBuf>) -> Result<(), Error> {
    let metadata = fs::metadata(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })?;

    if metadata.is_file() {
        if path
            .extension()
            .is_some_and(|extension| extension == "capnp")
        {
            files.push(path.to_owned());
        }
        return Ok(());
    }

    for entry in fs::read_dir(path).map_err(|source| Error::Io {
        path: path.to_owned(),
        source,
    })? {
        let entry = entry.map_err(|source| Error::Io {
            path: path.to_owned(),
            source,
        })?;
        discover_rec(&entry.path(), files)?;
    }

    Ok(())
}

fn compile_with_capnp(
    files: &[PathBuf],
    source_import_roots: &[PathBuf],
    import_paths: &[PathBuf],
) -> Result<(), Error> {
    let mut command = Command::new("capnp");
    command.arg("compile").arg("-o-");

    for path in source_import_roots {
        command.arg(format!("-I{}", path.display()));
    }

    for path in import_paths {
        command.arg(format!("-I{}", path.display()));
    }

    for file in files {
        command.arg(file);
    }

    let printable = format!("{command:?}");
    let output = command.output().map_err(|source| Error::Io {
        path: PathBuf::from("capnp"),
        source,
    })?;

    if output.status.success() {
        Ok(())
    } else {
        Err(Error::CapnpFailed {
            command: printable,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn discover_glob(
    pattern: &str,
    files: &mut Vec<PathBuf>,
    import_roots: &mut BTreeSet<PathBuf>,
) -> Result<bool, Error> {
    let base = glob_base(pattern);
    let mut candidates = Vec::new();
    if base.exists() {
        discover_rec(&base, &mut candidates)?;
    }

    let mut matched = false;
    for candidate in candidates {
        if glob_matches(pattern, &path_to_slash(&candidate)) {
            if let Some(parent) = candidate.parent() {
                import_roots.insert(parent.to_owned());
            }
            files.push(candidate);
            matched = true;
        }
    }

    Ok(matched)
}

fn glob_base(pattern: &str) -> PathBuf {
    let path = Path::new(pattern);
    let mut base = PathBuf::new();

    for component in path.components() {
        let text = component.as_os_str().to_string_lossy();
        if has_glob_meta(&text) {
            break;
        }
        base.push(component.as_os_str());
    }

    if base.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        base
    }
}

fn has_glob_meta(value: &str) -> bool {
    value.contains('*') || value.contains('?')
}

fn path_to_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn glob_matches(pattern: &str, candidate: &str) -> bool {
    let pattern_parts = pattern.split('/').collect::<Vec<_>>();
    let candidate_parts = candidate.split('/').collect::<Vec<_>>();
    glob_parts_match(&pattern_parts, &candidate_parts)
}

fn glob_parts_match(pattern: &[&str], candidate: &[&str]) -> bool {
    match (pattern.split_first(), candidate.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&"**", rest)), _) => {
            glob_parts_match(rest, candidate)
                || candidate
                    .split_first()
                    .is_some_and(|(_, tail)| glob_parts_match(pattern, tail))
        }
        (Some((part, rest)), Some((candidate_part, candidate_rest))) => {
            glob_component_match(part, candidate_part) && glob_parts_match(rest, candidate_rest)
        }
        (Some(_), None) => false,
    }
}

fn glob_component_match(pattern: &str, candidate: &str) -> bool {
    glob_component_match_inner(
        &pattern.chars().collect::<Vec<_>>(),
        &candidate.chars().collect::<Vec<_>>(),
    )
}

fn glob_component_match_inner(pattern: &[char], candidate: &[char]) -> bool {
    match (pattern.split_first(), candidate.split_first()) {
        (None, None) => true,
        (None, Some(_)) => false,
        (Some((&'*', rest)), _) => {
            glob_component_match_inner(rest, candidate)
                || candidate
                    .split_first()
                    .is_some_and(|(_, tail)| glob_component_match_inner(pattern, tail))
        }
        (Some((&'?', rest)), Some((_, candidate_rest))) => {
            glob_component_match_inner(rest, candidate_rest)
        }
        (Some((expected, rest)), Some((actual, candidate_rest))) if expected == actual => {
            glob_component_match_inner(rest, candidate_rest)
        }
        _ => false,
    }
}

struct GitExport {
    root: PathBuf,
    before_dir: PathBuf,
    after_dir: PathBuf,
}

impl GitExport {
    fn new(
        before_ref: &str,
        after_ref: &str,
        paths: &[String],
        import_paths: &[PathBuf],
    ) -> Result<Self, Error> {
        let root = make_temp_dir()?;
        let before_dir = root.join("before");
        let after_dir = root.join("after");
        fs::create_dir_all(&before_dir).map_err(|source| Error::Io {
            path: before_dir.clone(),
            source,
        })?;
        fs::create_dir_all(&after_dir).map_err(|source| Error::Io {
            path: after_dir.clone(),
            source,
        })?;

        let archive_roots = archive_roots(paths, import_paths);
        export_git_ref(before_ref, &before_dir, &archive_roots)?;
        export_git_ref(after_ref, &after_dir, &archive_roots)?;

        Ok(Self {
            root,
            before_dir,
            after_dir,
        })
    }
}

impl Drop for GitExport {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct WorktreeExport {
    root: PathBuf,
    before_dir: PathBuf,
    after_dir: PathBuf,
}

impl WorktreeExport {
    fn new(compare_ref: &str, paths: &[String], import_paths: &[PathBuf]) -> Result<Self, Error> {
        let root = make_temp_dir()?;
        let before_dir = root.join("before");
        let after_dir = root.join("after");
        fs::create_dir_all(&before_dir).map_err(|source| Error::Io {
            path: before_dir.clone(),
            source,
        })?;
        fs::create_dir_all(&after_dir).map_err(|source| Error::Io {
            path: after_dir.clone(),
            source,
        })?;

        let archive_roots = archive_roots(paths, import_paths);
        export_git_ref(compare_ref, &before_dir, &archive_roots)?;
        let repo_root = git_repo_root()?;
        copy_worktree_paths(&repo_root, &after_dir, &archive_roots)?;

        Ok(Self {
            root,
            before_dir,
            after_dir,
        })
    }
}

impl Drop for WorktreeExport {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn make_temp_dir() -> Result<PathBuf, Error> {
    let mut root = std::env::temp_dir();
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    root.push(format!("captain-git-{}-{nanos}", std::process::id()));
    fs::create_dir_all(&root).map_err(|source| Error::Io {
        path: root.clone(),
        source,
    })?;
    Ok(root)
}

fn git_repo_root() -> Result<PathBuf, Error> {
    let mut command = Command::new("git");
    command.arg("rev-parse").arg("--show-toplevel");
    let printable = format!("{command:?}");
    let output = command.output().map_err(|source| Error::Io {
        path: PathBuf::from("git"),
        source,
    })?;

    if !output.status.success() {
        return Err(Error::CommandFailed {
            command: printable,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }

    Ok(PathBuf::from(
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
    ))
}

fn export_git_ref(reference: &str, destination: &Path, paths: &[PathBuf]) -> Result<(), Error> {
    let archive = destination.with_extension("tar");
    let existing_paths = existing_git_paths(reference, paths)?;

    if !paths.is_empty() && existing_paths.is_empty() {
        return Ok(());
    }

    let mut archive_command = Command::new("git");
    archive_command
        .arg("archive")
        .arg("--format=tar")
        .arg(format!("--output={}", archive.display()))
        .arg(reference);
    if !existing_paths.is_empty() {
        archive_command.arg("--");
        archive_command.args(existing_paths);
    }
    run_command(archive_command)?;

    let mut tar_command = Command::new("tar");
    tar_command
        .arg("-xf")
        .arg(&archive)
        .arg("-C")
        .arg(destination);
    let result = run_command(tar_command);
    let _ = fs::remove_file(&archive);
    result
}

fn existing_git_paths(reference: &str, paths: &[PathBuf]) -> Result<Vec<PathBuf>, Error> {
    if paths.is_empty() {
        return Ok(Vec::new());
    }

    verify_git_tree(reference)?;

    let mut existing = Vec::new();

    for path in paths {
        let Some(path) = git_relative_path(path) else {
            continue;
        };
        if path == Path::new(".") {
            existing.push(path);
            continue;
        }
        let mut command = Command::new("git");
        command
            .arg("cat-file")
            .arg("-e")
            .arg(format!("{reference}:{}", path_to_slash(&path)));

        let output = command.output().map_err(|source| Error::Io {
            path: PathBuf::from("git"),
            source,
        })?;

        if output.status.success() {
            existing.push(path);
        }
    }

    Ok(existing)
}

fn verify_git_tree(reference: &str) -> Result<(), Error> {
    let mut command = Command::new("git");
    command
        .arg("rev-parse")
        .arg("--verify")
        .arg("--quiet")
        .arg(format!("{reference}^{{tree}}"));
    run_command(command)
}

fn run_command(mut command: Command) -> Result<(), Error> {
    let printable = format!("{command:?}");
    let output = command.output().map_err(|source| Error::Io {
        path: command.get_program().into(),
        source,
    })?;

    if output.status.success() {
        Ok(())
    } else {
        Err(Error::CommandFailed {
            command: printable,
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

fn copy_worktree(source: &Path, destination: &Path) -> Result<(), Error> {
    for entry in fs::read_dir(source).map_err(|source_error| Error::Io {
        path: source.to_owned(),
        source: source_error,
    })? {
        let entry = entry.map_err(|source_error| Error::Io {
            path: source.to_owned(),
            source: source_error,
        })?;
        let file_name = entry.file_name();
        if should_skip_worktree_entry(&file_name.to_string_lossy()) {
            continue;
        }

        let source_path = entry.path();
        let destination_path = destination.join(&file_name);
        let metadata = entry.metadata().map_err(|source_error| Error::Io {
            path: source_path.clone(),
            source: source_error,
        })?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination_path).map_err(|source_error| Error::Io {
                path: destination_path.clone(),
                source: source_error,
            })?;
            copy_worktree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path).map_err(|source_error| Error::Io {
                path: source_path,
                source: source_error,
            })?;
        }
    }

    Ok(())
}

fn copy_worktree_paths(source: &Path, destination: &Path, paths: &[PathBuf]) -> Result<(), Error> {
    if paths.is_empty() {
        return copy_worktree(source, destination);
    }

    for path in paths {
        let Some(path) = git_relative_path(path) else {
            continue;
        };
        let source_path = source.join(&path);
        if !source_path.exists() {
            continue;
        }
        let destination_path = destination.join(&path);
        let metadata = fs::metadata(&source_path).map_err(|source_error| Error::Io {
            path: source_path.clone(),
            source: source_error,
        })?;

        if metadata.is_dir() {
            fs::create_dir_all(&destination_path).map_err(|source_error| Error::Io {
                path: destination_path.clone(),
                source: source_error,
            })?;
            copy_worktree(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            if let Some(parent) = destination_path.parent() {
                fs::create_dir_all(parent).map_err(|source_error| Error::Io {
                    path: parent.to_owned(),
                    source: source_error,
                })?;
            }
            fs::copy(&source_path, &destination_path).map_err(|source_error| Error::Io {
                path: source_path,
                source: source_error,
            })?;
        }
    }

    Ok(())
}

fn should_skip_worktree_entry(name: &str) -> bool {
    matches!(name, ".git" | "target")
}

fn archive_roots(paths: &[String], import_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut roots = BTreeSet::new();

    for path in paths {
        roots.insert(materialization_root(path));
    }

    for path in import_paths {
        if !path.is_absolute() {
            roots.insert(path.clone());
        }
    }

    roots
        .into_iter()
        .filter_map(|path| git_relative_path(&path))
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn materialization_root(path: &str) -> PathBuf {
    if has_glob_meta(path) {
        glob_base(path)
    } else {
        PathBuf::from(path)
    }
}

fn git_relative_path(path: &Path) -> Option<PathBuf> {
    if path.is_absolute() {
        return None;
    }

    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(part) => normalized.push(part),
            _ => return None,
        }
    }

    if normalized.as_os_str().is_empty() {
        Some(PathBuf::from("."))
    } else {
        Some(normalized)
    }
}

fn prefix_sources(root: &Path, sources: &[String]) -> Vec<String> {
    sources
        .iter()
        .map(|source| prefix_string_path(root, source))
        .collect()
}

fn prefix_import_paths(root: &Path, import_paths: &[PathBuf]) -> Vec<PathBuf> {
    import_paths
        .iter()
        .map(|path| {
            if path.is_absolute() {
                path.clone()
            } else {
                root.join(path)
            }
        })
        .collect()
}

fn prefix_string_path(root: &Path, path: &str) -> String {
    let path = Path::new(path);
    if path.is_absolute() {
        path.display().to_string()
    } else {
        root.join(path).display().to_string()
    }
}

fn lint_schema_sets(before: &SchemaSet, after: &SchemaSet) -> Vec<Lint> {
    let mut lints = Vec::new();
    lints.extend(before.lints.clone());
    lints.extend(after.lints.clone());
    lint_file_ids("before", &before.files, &mut lints);
    lint_file_ids("after", &after.files, &mut lints);
    lint_file_id_changes(before, after, &mut lints);
    lint_file_id_path_changes(before, after, &mut lints);
    lint_removed_files(before, after, &mut lints);
    lint_node_path_changes(before, after, &mut lints);
    lint_any_pointer_types(after, &mut lints);
    lints
}

fn lint_any_pointer_types(schema: &SchemaSet, lints: &mut Vec<Lint>) {
    for (node_name, node) in &schema.snapshot.nodes {
        match node.kind {
            NodeKind::Struct => {
                for (ordinal, field) in &node.fields {
                    if !contains_any_pointer(&field.ty) {
                        continue;
                    }
                    lints.push(Lint {
                        severity: LintSeverity::Warning,
                        path: format!("{node_name}.field[{ordinal}]"),
                        reason: "field uses AnyPointer".to_owned(),
                        detail: Some(format!("{}: {}", field.name, field.ty)),
                    });
                }
            }
            NodeKind::Interface => {
                for (ordinal, method) in &node.methods {
                    lint_method_any_pointer(node_name, *ordinal, method, lints);
                }
            }
            NodeKind::Enum => {}
        }
    }
}

fn lint_method_any_pointer(node_name: &str, ordinal: u32, method: &Method, lints: &mut Vec<Lint>) {
    let (params, results) = method
        .signature
        .split_once("->")
        .unwrap_or((&method.signature, ""));

    if contains_any_pointer(params) {
        lints.push(Lint {
            severity: LintSeverity::Warning,
            path: format!("{node_name}.method[{ordinal}]"),
            reason: "method parameter uses AnyPointer".to_owned(),
            detail: Some(format!("{} {}", method.name, method.signature)),
        });
    }

    if contains_any_pointer(results) {
        lints.push(Lint {
            severity: LintSeverity::Warning,
            path: format!("{node_name}.method[{ordinal}]"),
            reason: "method result uses AnyPointer".to_owned(),
            detail: Some(format!("{} {}", method.name, method.signature)),
        });
    }
}

fn contains_any_pointer(value: &str) -> bool {
    value
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_'))
        .any(|part| part == "AnyPointer")
}

fn lint_file_ids(side: &str, files: &[SchemaFile], lints: &mut Vec<Lint>) {
    let mut by_id: BTreeMap<&str, Vec<&SchemaFile>> = BTreeMap::new();

    for file in files {
        let path = format!("{side}:{}", file.relative_path);
        let Some(file_id) = &file.file_id else {
            lints.push(Lint {
                severity: LintSeverity::Error,
                path,
                reason: "file is missing a file id".to_owned(),
                detail: None,
            });
            continue;
        };
        by_id.entry(file_id).or_default().push(file);
    }

    for (file_id, duplicates) in by_id {
        if duplicates.len() < 2 {
            continue;
        }
        lints.push(Lint {
            severity: LintSeverity::Error,
            path: format!("{side}:{file_id}"),
            reason: "file id is used by multiple files".to_owned(),
            detail: Some(
                duplicates
                    .into_iter()
                    .map(|file| file.relative_path.clone())
                    .collect::<Vec<_>>()
                    .join(", "),
            ),
        });
    }
}

fn lint_file_id_changes(before: &SchemaSet, after: &SchemaSet, lints: &mut Vec<Lint>) {
    let before_by_path = before
        .files
        .iter()
        .map(|file| (file.relative_path.as_str(), file))
        .collect::<BTreeMap<_, _>>();

    for after_file in &after.files {
        let Some(before_file) = before_by_path.get(after_file.relative_path.as_str()) else {
            continue;
        };
        let (Some(before_id), Some(after_id)) = (&before_file.file_id, &after_file.file_id) else {
            continue;
        };
        if before_id == after_id {
            continue;
        }
        lints.push(Lint {
            severity: LintSeverity::Error,
            path: after_file.relative_path.clone(),
            reason: "file id changed".to_owned(),
            detail: Some(format!("{before_id} -> {after_id}")),
        });
    }
}

fn lint_file_id_path_changes(before: &SchemaSet, after: &SchemaSet, lints: &mut Vec<Lint>) {
    let before_by_id = files_by_id(&before.files);
    let after_by_id = files_by_id(&after.files);

    for (file_id, before_file) in before_by_id {
        let Some(after_file) = after_by_id.get(file_id) else {
            continue;
        };
        if before_file.relative_path == after_file.relative_path {
            continue;
        }
        lints.push(Lint {
            severity: LintSeverity::Error,
            path: file_id.to_owned(),
            reason: "file id moved to a different path".to_owned(),
            detail: Some(format!(
                "{} -> {}",
                before_file.relative_path, after_file.relative_path
            )),
        });
    }
}

fn lint_removed_files(before: &SchemaSet, after: &SchemaSet, lints: &mut Vec<Lint>) {
    let after_paths = after
        .files
        .iter()
        .map(|file| file.relative_path.as_str())
        .collect::<BTreeSet<_>>();
    let after_ids = after
        .files
        .iter()
        .filter_map(|file| file.file_id.as_deref())
        .collect::<BTreeSet<_>>();

    for before_file in &before.files {
        if after_paths.contains(before_file.relative_path.as_str()) {
            continue;
        }
        if before_file
            .file_id
            .as_deref()
            .is_some_and(|file_id| after_ids.contains(file_id))
        {
            continue;
        }
        lints.push(Lint {
            severity: LintSeverity::Error,
            path: before_file.relative_path.clone(),
            reason: "schema file was removed".to_owned(),
            detail: before_file.file_id.clone(),
        });
    }
}

fn lint_node_path_changes(before: &SchemaSet, after: &SchemaSet, lints: &mut Vec<Lint>) {
    for (node_name, before_path) in &before.node_paths {
        let Some(after_path) = after.node_paths.get(node_name) else {
            continue;
        };
        if before_path == after_path {
            continue;
        }
        lints.push(Lint {
            severity: LintSeverity::Error,
            path: node_name.clone(),
            reason: "node moved to a different file".to_owned(),
            detail: Some(format!("{before_path} -> {after_path}")),
        });
    }
}

fn files_by_id(files: &[SchemaFile]) -> BTreeMap<&str, &SchemaFile> {
    files
        .iter()
        .filter_map(|file| file.file_id.as_deref().map(|file_id| (file_id, file)))
        .collect()
}

fn lint_field_default_changes(
    node_name: &str,
    before_node: &Node,
    after_node: &Node,
    lints: &mut Vec<Lint>,
) {
    if before_node.kind != NodeKind::Struct {
        return;
    }

    for (ordinal, before_field) in &before_node.fields {
        let Some(after_field) = after_node.fields.get(ordinal) else {
            continue;
        };
        if normalize_type(&before_field.ty) != normalize_type(&after_field.ty) {
            continue;
        }
        if normalize_default(before_field.default.as_deref())
            == normalize_default(after_field.default.as_deref())
        {
            continue;
        }
        lints.push(Lint {
            severity: LintSeverity::Error,
            path: format!("{node_name}.field[{ordinal}]"),
            reason: "field default value changed".to_owned(),
            detail: Some(format!(
                "{} -> {}",
                before_field.default.as_deref().unwrap_or("<none>"),
                after_field.default.as_deref().unwrap_or("<none>")
            )),
        });
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct FileMetadata {
    file_id: Option<String>,
    lints: Vec<Lint>,
}

fn parse_file_metadata(source: &str) -> FileMetadata {
    let mut file_id = None;
    let mut lints = Vec::new();

    for statement in statements(source) {
        if !statement.trim().starts_with('@') {
            continue;
        }

        let Some(parsed) = parse_file_id_statement(&statement) else {
            lints.push(Lint {
                severity: LintSeverity::Error,
                path: String::new(),
                reason: "file id declaration is malformed".to_owned(),
                detail: Some(statement),
            });
            continue;
        };

        if parsed == "@0x0000000000000000" {
            lints.push(Lint {
                severity: LintSeverity::Error,
                path: String::new(),
                reason: "file id must not be zero".to_owned(),
                detail: Some(parsed.clone()),
            });
        }

        if let Some(existing) = &file_id {
            lints.push(Lint {
                severity: LintSeverity::Error,
                path: String::new(),
                reason: "file id is declared multiple times".to_owned(),
                detail: Some(format!("{existing} and {parsed}")),
            });
        } else {
            file_id = Some(parsed);
        }
    }

    FileMetadata { file_id, lints }
}

fn parse_file_id_statement(statement: &str) -> Option<String> {
    let statement = statement.trim().trim_end_matches(';').trim();
    let id = statement.strip_prefix('@')?;
    let hex = id.strip_prefix("0x")?;
    if hex.len() != 16 || !hex.chars().all(|ch| ch.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("@0x{}", hex.to_ascii_lowercase()))
}

fn common_file_root(files: &[PathBuf]) -> PathBuf {
    let mut parents = files
        .iter()
        .filter_map(|file| file.parent().map(Path::to_path_buf));
    let Some(mut root) = parents.next() else {
        return PathBuf::new();
    };

    for parent in parents {
        while !parent.starts_with(&root) {
            if !root.pop() {
                return PathBuf::new();
            }
        }
    }

    root
}

fn relative_schema_path(file: &Path, root: &Path) -> String {
    file.strip_prefix(root)
        .unwrap_or(file)
        .to_string_lossy()
        .replace('\\', "/")
}

fn compare_struct_fields(
    node_name: &str,
    before_node: &Node,
    after_node: &Node,
    violations: &mut Vec<Violation>,
    lints: &mut Vec<Lint>,
) {
    if before_node.kind != NodeKind::Struct {
        return;
    }

    for (ordinal, before_field) in &before_node.fields {
        let path = format!("{node_name}.field[{ordinal}]");
        let Some(after_field) = after_node.fields.get(ordinal) else {
            violations.push(Violation {
                path,
                reason: "field was removed".to_owned(),
                before: Some(format!("{}: {}", before_field.name, before_field.ty)),
                after: None,
            });
            continue;
        };

        if normalize_type(&before_field.ty) != normalize_type(&after_field.ty) {
            violations.push(Violation {
                path,
                reason: "field type changed".to_owned(),
                before: Some(format!("{}: {}", before_field.name, before_field.ty)),
                after: Some(format!("{}: {}", after_field.name, after_field.ty)),
            });
        } else if before_field.name != after_field.name {
            lints.push(Lint {
                severity: LintSeverity::Error,
                path,
                reason: "field name changed".to_owned(),
                detail: Some(format!("{} -> {}", before_field.name, after_field.name)),
            });
        }
    }
}

fn compare_enum_values(
    node_name: &str,
    before_node: &Node,
    after_node: &Node,
    violations: &mut Vec<Violation>,
    lints: &mut Vec<Lint>,
) {
    if before_node.kind != NodeKind::Enum {
        return;
    }

    for (ordinal, before_value) in &before_node.enum_values {
        let path = format!("{node_name}.enum[{ordinal}]");
        let Some(after_value) = after_node.enum_values.get(ordinal) else {
            violations.push(Violation {
                path,
                reason: "enum value was removed".to_owned(),
                before: Some(before_value.name.clone()),
                after: None,
            });
            continue;
        };

        if before_value.name != after_value.name {
            lints.push(Lint {
                severity: LintSeverity::Error,
                path,
                reason: "enum value name changed".to_owned(),
                detail: Some(format!("{} -> {}", before_value.name, after_value.name)),
            });
        }
    }
}

fn compare_methods(
    node_name: &str,
    before_node: &Node,
    after_node: &Node,
    violations: &mut Vec<Violation>,
    lints: &mut Vec<Lint>,
) {
    if before_node.kind != NodeKind::Interface {
        return;
    }

    for (ordinal, before_method) in &before_node.methods {
        let path = format!("{node_name}.method[{ordinal}]");
        let Some(after_method) = after_node.methods.get(ordinal) else {
            violations.push(Violation {
                path,
                reason: "method was removed".to_owned(),
                before: Some(format!(
                    "{} {}",
                    before_method.name, before_method.signature
                )),
                after: None,
            });
            continue;
        };

        if normalize_signature(&before_method.signature)
            != normalize_signature(&after_method.signature)
        {
            violations.push(Violation {
                path,
                reason: "method signature changed".to_owned(),
                before: Some(format!(
                    "{} {}",
                    before_method.name, before_method.signature
                )),
                after: Some(format!("{} {}", after_method.name, after_method.signature)),
            });
        } else if before_method.name != after_method.name {
            lints.push(Lint {
                severity: LintSeverity::Error,
                path,
                reason: "method name changed".to_owned(),
                detail: Some(format!("{} -> {}", before_method.name, after_method.name)),
            });
        }
    }
}

fn statements(source: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    let stripped = strip_comments(source);
    let mut chars = stripped.chars().peekable();

    while let Some(ch) = chars.next() {
        match ch {
            '{' => {
                current.push(ch);
                push_statement(&mut result, &mut current);
            }
            '}' => {
                push_statement(&mut result, &mut current);
                result.push("}".to_owned());
            }
            ';' => {
                current.push(ch);
                push_statement(&mut result, &mut current);
            }
            '\n' | '\r' | '\t' => current.push(' '),
            _ => current.push(ch),
        }

        while chars.peek().is_some_and(|next| next.is_whitespace()) && current.ends_with(' ') {
            chars.next();
        }
    }

    push_statement(&mut result, &mut current);
    result
}

fn push_statement(result: &mut Vec<String>, current: &mut String) {
    let statement = current.trim();
    if !statement.is_empty() {
        result.push(statement.to_owned());
    }
    current.clear();
}

fn strip_comments(source: &str) -> String {
    source
        .lines()
        .map(|line| {
            let hash = line.find('#');
            let slash = line.find("//");
            let end = [hash, slash]
                .into_iter()
                .flatten()
                .min()
                .unwrap_or(line.len());
            &line[..end]
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn parse_node_start(statement: &str) -> Option<(NodeKind, String)> {
    let prefix = statement.strip_suffix('{')?.trim();
    let mut parts = prefix.split_whitespace();
    let kind = match parts.next()? {
        "struct" => NodeKind::Struct,
        "enum" => NodeKind::Enum,
        "interface" => NodeKind::Interface,
        _ => return None,
    };
    let name = parts.next()?.trim().to_owned();
    Some((kind, name))
}

fn parse_field(statement: &str) -> Option<(u32, Field)> {
    if statement.starts_with("using ")
        || statement.starts_with("const ")
        || statement.starts_with("annotation ")
        || statement.starts_with("union ")
    {
        return None;
    }

    let at = statement.find('@')?;
    let colon = statement[at..].find(':')? + at;
    let name = statement[..at].trim();
    if name.is_empty() {
        return None;
    }

    let ordinal = parse_ordinal(&statement[at + 1..colon])?;
    let value = statement[colon + 1..].trim().trim_end_matches(';').trim();
    let (ty, default) = parse_field_type_and_default(value)?;

    if ty.is_empty() {
        None
    } else {
        Some((
            ordinal,
            Field {
                name: name.to_owned(),
                ty,
                default,
            },
        ))
    }
}

fn parse_field_type_and_default(value: &str) -> Option<(String, Option<String>)> {
    let mut parts = value.splitn(2, '=');
    let ty = parts.next()?.trim().to_owned();
    let default = parts
        .next()
        .map(str::trim)
        .filter(|default| !default.is_empty())
        .map(str::to_owned);
    Some((ty, default))
}

fn parse_enum_value(statement: &str) -> Option<(u32, EnumValue)> {
    let at = statement.find('@')?;
    let name = statement[..at].trim();
    if name.is_empty() {
        return None;
    }

    let ordinal = parse_ordinal(statement[at + 1..].trim_end_matches(';'))?;
    Some((
        ordinal,
        EnumValue {
            name: name.to_owned(),
        },
    ))
}

fn parse_method(statement: &str) -> Option<(u32, Method)> {
    let at = statement.find('@')?;
    let name = statement[..at].trim();
    if name.is_empty() {
        return None;
    }

    let rest = statement[at + 1..].trim_end_matches(';').trim();
    let ordinal_end = rest.find(|ch: char| !ch.is_ascii_digit())?;
    let ordinal = parse_ordinal(&rest[..ordinal_end])?;
    let signature = rest[ordinal_end..].trim().to_owned();

    Some((
        ordinal,
        Method {
            name: name.to_owned(),
            signature,
        },
    ))
}

fn parse_ordinal(input: &str) -> Option<u32> {
    input.trim().parse().ok()
}

fn normalize_type(ty: &str) -> String {
    ty.split_whitespace().collect::<String>()
}

fn normalize_signature(signature: &str) -> String {
    signature.split_whitespace().collect::<String>()
}

fn normalize_default(default: Option<&str>) -> Option<String> {
    default.map(|value| value.split_whitespace().collect::<String>())
}

pub fn format_report(report: &Report) -> String {
    if report.violations.is_empty() && report.lints.is_empty() {
        return "compatible: no backwards-incompatible schema changes found".to_owned();
    }

    let mut output = String::new();
    for violation in &report.violations {
        output.push_str(&format!("incompatible: {}\n", violation.path));
        output.push_str(&format!("  reason: {}\n", violation.reason));
        if let Some(before) = &violation.before {
            output.push_str(&format!("  before: {before}\n"));
        }
        if let Some(after) = &violation.after {
            output.push_str(&format!("  after: {after}\n"));
        }
    }
    for lint in &report.lints {
        output.push_str(&format!("lint: {}\n", lint.path));
        output.push_str(&format!("  severity: {}\n", lint.severity.as_str()));
        output.push_str(&format!("  reason: {}\n", lint.reason));
        if let Some(detail) = &lint.detail {
            output.push_str(&format!("  detail: {detail}\n"));
        }
    }
    output.trim_end().to_owned()
}

impl LintSeverity {
    fn as_str(self) -> &'static str {
        match self {
            LintSeverity::Error => "error",
            LintSeverity::Warning => "warning",
        }
    }
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<CheckConfig, Error> {
    let mut args = args.into_iter().peekable();
    let _program = args.next();

    match args.next().as_deref() {
        Some("check") => {}
        _ => return Err(Error::Usage(usage())),
    }

    let mut before = None;
    let mut after = None;
    let mut before_ref = None;
    let mut after_ref = None;
    let mut compare_ref = None;
    let mut paths = None;
    let mut import_paths = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--before" => push_sources(&mut before, &mut args, "--before")?,
            "--after" => push_sources(&mut after, &mut args, "--after")?,
            "--before-ref" => before_ref = Some(take_value(&mut args, "--before-ref")?),
            "--after-ref" => after_ref = Some(take_value(&mut args, "--after-ref")?),
            "--compare-ref" => compare_ref = Some(take_value(&mut args, "--compare-ref")?),
            "--path" => push_sources(&mut paths, &mut args, "--path")?,
            "-I" | "--import-path" => {
                let Some(path) = args.next() else {
                    return Err(Error::Usage(format!(
                        "{arg} requires a path\n\n{}",
                        usage()
                    )));
                };
                import_paths.push(PathBuf::from(path));
            }
            _ if arg.starts_with("-I") && arg.len() > 2 => {
                import_paths.push(PathBuf::from(&arg[2..]));
            }
            _ => {
                return Err(Error::Usage(format!(
                    "unknown argument: {arg}\n\n{}",
                    usage()
                )));
            }
        }
    }

    Ok(CheckConfig {
        before: before.unwrap_or_default(),
        after: after.unwrap_or_default(),
        before_ref,
        after_ref,
        compare_ref,
        paths: paths.unwrap_or_default(),
        import_paths,
    })
}

pub fn usage() -> String {
    concat!(
        "usage:\n",
        "  captain check --before <path|glob>... --after <path|glob>... [-I <path>]...\n",
        "  captain check --before-ref <ref> --after-ref <ref> --path <path|glob>... [-I <path>]...\n",
        "  captain check --compare-ref <ref> --path <path|glob>... [-I <path>]..."
    )
    .to_owned()
}

fn take_value(
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<String, Error> {
    let Some(value) = args.next() else {
        return Err(Error::Usage(format!(
            "{flag} requires a value\n\n{}",
            usage()
        )));
    };
    if value.starts_with('-') {
        return Err(Error::Usage(format!(
            "{flag} requires a value\n\n{}",
            usage()
        )));
    }
    Ok(value)
}

fn push_sources(
    target: &mut Option<Vec<String>>,
    args: &mut std::iter::Peekable<impl Iterator<Item = String>>,
    flag: &str,
) -> Result<(), Error> {
    let mut pushed = false;

    while let Some(next) = args.peek() {
        if next.starts_with('-') {
            break;
        }
        let source = args.next().expect("peeked argument must exist");
        target.get_or_insert_with(Vec::new).push(source);
        pushed = true;
    }

    if pushed {
        Ok(())
    } else {
        return Err(Error::Usage(format!(
            "{flag} requires a path or glob\n\n{}",
            usage()
        )));
    }
}

pub fn violation_paths(report: &Report) -> BTreeSet<String> {
    report
        .violations
        .iter()
        .map(|violation| violation.path.clone())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn archive_roots_use_glob_bases_and_relative_import_paths() {
        let roots = archive_roots(
            &["schemas/**/*.capnp".to_owned(), "api/user.capnp".to_owned()],
            &[
                PathBuf::from("schemas"),
                PathBuf::from("shared/capnp"),
                PathBuf::from("/usr/include/capnp"),
            ],
        );

        assert_eq!(
            roots,
            vec![
                PathBuf::from("api/user.capnp"),
                PathBuf::from("schemas"),
                PathBuf::from("shared/capnp"),
            ]
        );
    }

    #[test]
    fn archive_roots_reject_parent_paths() {
        let roots = archive_roots(
            &[
                "schemas/**/*.capnp".to_owned(),
                "../outside/**/*.capnp".to_owned(),
            ],
            &[PathBuf::from("./schemas"), PathBuf::from("../shared")],
        );

        assert_eq!(roots, vec![PathBuf::from("schemas")]);
    }

    #[test]
    fn lints_missing_file_ids_before_compile() {
        let before = SchemaSet {
            snapshot: Snapshot::default(),
            node_paths: BTreeMap::new(),
            files: vec![SchemaFile {
                path: PathBuf::from("old/user.capnp"),
                relative_path: "user.capnp".to_owned(),
                file_id: Some("@0xbf5147cbbecf40c1".to_owned()),
            }],
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: Snapshot::default(),
            node_paths: BTreeMap::new(),
            files: vec![SchemaFile {
                path: PathBuf::from("new/user.capnp"),
                relative_path: "user.capnp".to_owned(),
                file_id: None,
            }],
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: after:user.capnp\n",
                "  severity: error\n",
                "  reason: file is missing a file id"
            )
        );
    }

    #[test]
    fn lints_duplicate_file_ids() {
        let before = SchemaSet::default();
        let after = SchemaSet {
            snapshot: Snapshot::default(),
            node_paths: BTreeMap::new(),
            files: vec![
                SchemaFile {
                    path: PathBuf::from("new/payment.capnp"),
                    relative_path: "payment.capnp".to_owned(),
                    file_id: Some("@0xbf5147cbbecf40c1".to_owned()),
                },
                SchemaFile {
                    path: PathBuf::from("new/user.capnp"),
                    relative_path: "user.capnp".to_owned(),
                    file_id: Some("@0xbf5147cbbecf40c1".to_owned()),
                },
            ],
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: after:@0xbf5147cbbecf40c1\n",
                "  severity: error\n",
                "  reason: file id is used by multiple files\n",
                "  detail: payment.capnp, user.capnp"
            )
        );
    }

    #[test]
    fn lints_file_id_changes_for_same_relative_path() {
        let before = SchemaSet {
            snapshot: Snapshot::default(),
            node_paths: BTreeMap::new(),
            files: vec![SchemaFile {
                path: PathBuf::from("old/user.capnp"),
                relative_path: "user.capnp".to_owned(),
                file_id: Some("@0xbf5147cbbecf40c1".to_owned()),
            }],
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: Snapshot::default(),
            node_paths: BTreeMap::new(),
            files: vec![SchemaFile {
                path: PathBuf::from("new/user.capnp"),
                relative_path: "user.capnp".to_owned(),
                file_id: Some("@0xaf5147cbbecf40c1".to_owned()),
            }],
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: user.capnp\n",
                "  severity: error\n",
                "  reason: file id changed\n",
                "  detail: @0xbf5147cbbecf40c1 -> @0xaf5147cbbecf40c1"
            )
        );
    }

    #[test]
    fn lints_duplicate_node_names_without_overwriting_first_node() {
        let root = test_temp_dir("duplicate-node-name");
        let first = root.join("a.capnp");
        let second = root.join("b.capnp");
        fs::write(
            &first,
            concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "}\n"
            ),
        )
        .unwrap();
        fs::write(
            &second,
            concat!(
                "@0xaf5147cbbecf40c1;\n",
                "struct User {\n",
                "  email @0 :Text;\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[first, second]).unwrap();

        assert_eq!(set.snapshot.nodes["User"].fields[&0].name, "id");
        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "User".to_owned(),
                reason: "node name is defined in multiple files".to_owned(),
                detail: Some("a.capnp and b.capnp".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_duplicate_discovered_files() {
        let root = test_temp_dir("duplicate-discovered-file");
        let file = root.join("user.capnp");
        fs::write(
            &file,
            concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "}\n"
            ),
        )
        .unwrap();

        let sources =
            discover_capnp_sources(&[file.display().to_string(), file.display().to_string()])
                .unwrap();
        let set = snapshot_sources(&sources).unwrap();

        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "user.capnp".to_owned(),
                reason: "file was selected more than once".to_owned(),
                detail: None,
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_parser_collisions_without_overwriting_first_field() {
        let root = test_temp_dir("parser-collision");
        let file = root.join("user.capnp");
        fs::write(
            &file,
            concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "  email @0 :Text;\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[file]).unwrap();

        assert_eq!(set.snapshot.nodes["User"].fields[&0].name, "id");
        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "user.capnp:User.field[0]".to_owned(),
                reason: "field ordinal is defined multiple times".to_owned(),
                detail: Some("id and email".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_duplicate_field_names() {
        let root = test_temp_dir("duplicate-field-name");
        let file = root.join("user.capnp");
        fs::write(
            &file,
            concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  email @0 :Text;\n",
                "  email @1 :Text;\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[file]).unwrap();

        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "user.capnp:User.field[1]".to_owned(),
                reason: "field name is defined multiple times".to_owned(),
                detail: Some("email also used by field[0]".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_duplicate_enum_value_names() {
        let root = test_temp_dir("duplicate-enum-name");
        let file = root.join("status.capnp");
        fs::write(
            &file,
            concat!(
                "@0xbf5147cbbecf40c1;\n",
                "enum Status {\n",
                "  active @0;\n",
                "  active @1;\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[file]).unwrap();

        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "status.capnp:Status.enum[1]".to_owned(),
                reason: "enum value name is defined multiple times".to_owned(),
                detail: Some("active also used by enum[0]".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_duplicate_method_names() {
        let root = test_temp_dir("duplicate-method-name");
        let file = root.join("users.capnp");
        fs::write(
            &file,
            concat!(
                "@0xbf5147cbbecf40c1;\n",
                "interface Users {\n",
                "  get @0 (id :UInt64) -> (email :Text);\n",
                "  get @1 (id :UInt64) -> (email :Text);\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[file]).unwrap();

        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "users.capnp:Users.method[1]".to_owned(),
                reason: "method name is defined multiple times".to_owned(),
                detail: Some("get also used by method[0]".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_malformed_file_ids() {
        let root = test_temp_dir("malformed-file-id");
        let file = root.join("user.capnp");
        fs::write(
            &file,
            concat!(
                "@0xnot-an-id;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[file]).unwrap();

        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "user.capnp".to_owned(),
                reason: "file id declaration is malformed".to_owned(),
                detail: Some("@0xnot-an-id;".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_multiple_file_ids_in_one_file() {
        let root = test_temp_dir("multiple-file-id");
        let file = root.join("user.capnp");
        fs::write(
            &file,
            concat!(
                "@0xbf5147cbbecf40c1;\n",
                "@0xaf5147cbbecf40c1;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[file]).unwrap();

        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "user.capnp".to_owned(),
                reason: "file id is declared multiple times".to_owned(),
                detail: Some("@0xbf5147cbbecf40c1 and @0xaf5147cbbecf40c1".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_zero_file_ids() {
        let root = test_temp_dir("zero-file-id");
        let file = root.join("user.capnp");
        fs::write(
            &file,
            concat!(
                "@0x0000000000000000;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "}\n"
            ),
        )
        .unwrap();

        let set = snapshot_files(&[file]).unwrap();

        assert_eq!(
            set.lints,
            vec![Lint {
                severity: LintSeverity::Error,
                path: "user.capnp".to_owned(),
                reason: "file id must not be zero".to_owned(),
                detail: Some("@0x0000000000000000".to_owned()),
            }]
        );
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn lints_file_id_path_changes() {
        let before = SchemaSet {
            snapshot: Snapshot::default(),
            files: vec![SchemaFile {
                path: PathBuf::from("old/user.capnp"),
                relative_path: "user.capnp".to_owned(),
                file_id: Some("@0xbf5147cbbecf40c1".to_owned()),
            }],
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: Snapshot::default(),
            files: vec![SchemaFile {
                path: PathBuf::from("new/account.capnp"),
                relative_path: "account.capnp".to_owned(),
                file_id: Some("@0xbf5147cbbecf40c1".to_owned()),
            }],
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: @0xbf5147cbbecf40c1\n",
                "  severity: error\n",
                "  reason: file id moved to a different path\n",
                "  detail: user.capnp -> account.capnp"
            )
        );
    }

    #[test]
    fn lints_removed_schema_files() {
        let before = SchemaSet {
            snapshot: Snapshot::default(),
            files: vec![SchemaFile {
                path: PathBuf::from("old/legacy.capnp"),
                relative_path: "legacy.capnp".to_owned(),
                file_id: Some("@0xbf5147cbbecf40c1".to_owned()),
            }],
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };
        let after = SchemaSet::default();

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: legacy.capnp\n",
                "  severity: error\n",
                "  reason: schema file was removed\n",
                "  detail: @0xbf5147cbbecf40c1"
            )
        );
    }

    #[test]
    fn lints_node_path_changes() {
        let mut before_node_paths = BTreeMap::new();
        before_node_paths.insert("User".to_owned(), "user.capnp".to_owned());
        let mut after_node_paths = BTreeMap::new();
        after_node_paths.insert("User".to_owned(), "account.capnp".to_owned());
        let before = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: before_node_paths,
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xaf5147cbbecf40c1;\n",
                "struct User {\n",
                "  id @0 :UInt64;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: after_node_paths,
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: User\n",
                "  severity: error\n",
                "  reason: node moved to a different file\n",
                "  detail: user.capnp -> account.capnp"
            )
        );
    }

    #[test]
    fn lints_field_default_value_changes() {
        let before = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  active @0 :Bool = true;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  active @0 :Bool = false;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: User.field[0]\n",
                "  severity: error\n",
                "  reason: field default value changed\n",
                "  detail: true -> false"
            )
        );
    }

    #[test]
    fn lints_field_name_changes() {
        let before = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  email @0 :Text;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct User {\n",
                "  primaryEmail @0 :Text;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: User.field[0]\n",
                "  severity: error\n",
                "  reason: field name changed\n",
                "  detail: email -> primaryEmail"
            )
        );
    }

    #[test]
    fn lints_enum_value_name_changes() {
        let before = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "enum Status {\n",
                "  active @0;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "enum Status {\n",
                "  enabled @0;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: Status.enum[0]\n",
                "  severity: error\n",
                "  reason: enum value name changed\n",
                "  detail: active -> enabled"
            )
        );
    }

    #[test]
    fn lints_method_name_changes() {
        let before = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "interface Users {\n",
                "  get @0 (id :UInt64) -> (email :Text);\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "interface Users {\n",
                "  fetch @0 (id :UInt64) -> (email :Text);\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: Users.method[0]\n",
                "  severity: error\n",
                "  reason: method name changed\n",
                "  detail: get -> fetch"
            )
        );
    }

    #[test]
    fn lints_any_pointer_fields() {
        let before = SchemaSet::default();
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "struct Envelope {\n",
                "  payload @0 :AnyPointer;\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: Envelope.field[0]\n",
                "  severity: warning\n",
                "  reason: field uses AnyPointer\n",
                "  detail: payload: AnyPointer"
            )
        );
    }

    #[test]
    fn lints_any_pointer_method_parameters() {
        let before = SchemaSet::default();
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "interface Store {\n",
                "  put @0 (payload :AnyPointer) -> (ok :Bool);\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: Store.method[0]\n",
                "  severity: warning\n",
                "  reason: method parameter uses AnyPointer\n",
                "  detail: put (payload :AnyPointer) -> (ok :Bool)"
            )
        );
    }

    #[test]
    fn lints_any_pointer_method_results() {
        let before = SchemaSet::default();
        let after = SchemaSet {
            snapshot: parse_schema(concat!(
                "@0xbf5147cbbecf40c1;\n",
                "interface Store {\n",
                "  get @0 (id :UInt64) -> (payload :AnyPointer);\n",
                "}\n"
            )),
            files: Vec::new(),
            node_paths: BTreeMap::new(),
            lints: Vec::new(),
        };

        let report = compare(&before, &after);

        assert_eq!(
            format_report(&report),
            concat!(
                "lint: Store.method[0]\n",
                "  severity: warning\n",
                "  reason: method result uses AnyPointer\n",
                "  detail: get (id :UInt64) -> (payload :AnyPointer)"
            )
        );
    }

    fn test_temp_dir(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!("captain-{name}-{nanos}"));
        let _ = fs::remove_dir_all(&path);
        fs::create_dir_all(&path).unwrap();
        path
    }
}
