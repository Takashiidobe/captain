use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug)]
pub enum Error {
    Io { path: PathBuf, source: io::Error },
    CapnpFailed { command: String, stderr: String },
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
    pub import_paths: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub violations: Vec<Violation>,
}

impl Report {
    pub fn is_compatible(&self) -> bool {
        self.violations.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Violation {
    pub path: String,
    pub reason: String,
    pub before: Option<String>,
    pub after: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Snapshot {
    pub nodes: BTreeMap<String, Node>,
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
    let before_sources = discover_capnp_sources(&config.before)?;
    let after_sources = discover_capnp_sources(&config.after)?;

    compile_with_capnp(
        &before_sources.files,
        &before_sources.import_roots,
        &config.import_paths,
    )?;
    compile_with_capnp(
        &after_sources.files,
        &after_sources.import_roots,
        &config.import_paths,
    )?;

    let before = snapshot_files(&before_sources.files)?;
    let after = snapshot_files(&after_sources.files)?;

    Ok(compare(&before, &after))
}

pub fn compare(before: &Snapshot, after: &Snapshot) -> Report {
    let mut violations = Vec::new();

    for (node_name, before_node) in &before.nodes {
        let Some(after_node) = after.nodes.get(node_name) else {
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

        compare_struct_fields(node_name, before_node, after_node, &mut violations);
        compare_enum_values(node_name, before_node, after_node, &mut violations);
        compare_methods(node_name, before_node, after_node, &mut violations);
    }

    Report { violations }
}

pub fn snapshot_files(files: &[PathBuf]) -> Result<Snapshot, Error> {
    let mut snapshot = Snapshot::default();

    for file in files {
        let source = fs::read_to_string(file).map_err(|source| Error::Io {
            path: file.clone(),
            source,
        })?;
        let file_snapshot = parse_schema(&source);
        for (name, node) in file_snapshot.nodes {
            snapshot.nodes.insert(name, node);
        }
    }

    Ok(snapshot)
}

pub fn parse_schema(source: &str) -> Snapshot {
    let mut snapshot = Snapshot::default();
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
            snapshot.nodes.insert(
                full_name.clone(),
                Node {
                    kind,
                    fields: BTreeMap::new(),
                    enum_values: BTreeMap::new(),
                    methods: BTreeMap::new(),
                },
            );
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
                    node.fields.insert(ordinal, field);
                }
            }
            NodeKind::Enum => {
                if let Some((ordinal, value)) = parse_enum_value(&statement) {
                    node.enum_values.insert(ordinal, value);
                }
            }
            NodeKind::Interface => {
                if let Some((ordinal, method)) = parse_method(&statement) {
                    node.methods.insert(ordinal, method);
                }
            }
        }
    }

    snapshot
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiscoveredSources {
    pub files: Vec<PathBuf>,
    pub import_roots: Vec<PathBuf>,
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
    files.dedup();

    if files.is_empty() {
        Err(Error::NoSchemas(PathBuf::from(sources.join(", "))))
    } else {
        let mut import_roots = import_roots.into_iter().collect::<Vec<_>>();
        import_roots.sort();
        Ok(DiscoveredSources {
            files,
            import_roots,
        })
    }
}

pub fn discover_capnp_files(root: &Path) -> Result<Vec<PathBuf>, Error> {
    Ok(discover_capnp_sources(&[root.display().to_string()])?.files)
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

fn compare_struct_fields(
    node_name: &str,
    before_node: &Node,
    after_node: &Node,
    violations: &mut Vec<Violation>,
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
        }
    }
}

fn compare_enum_values(
    node_name: &str,
    before_node: &Node,
    after_node: &Node,
    violations: &mut Vec<Violation>,
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
            violations.push(Violation {
                path,
                reason: "enum ordinal was reused with a different name".to_owned(),
                before: Some(before_value.name.clone()),
                after: Some(after_value.name.clone()),
            });
        }
    }
}

fn compare_methods(
    node_name: &str,
    before_node: &Node,
    after_node: &Node,
    violations: &mut Vec<Violation>,
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
    let ty = statement[colon + 1..]
        .trim()
        .trim_end_matches(';')
        .split('=')
        .next()?
        .trim()
        .to_owned();

    if ty.is_empty() {
        None
    } else {
        Some((
            ordinal,
            Field {
                name: name.to_owned(),
                ty,
            },
        ))
    }
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

pub fn format_report(report: &Report) -> String {
    if report.is_compatible() {
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
    output.trim_end().to_owned()
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
    let mut import_paths = Vec::new();

    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--before" => push_sources(&mut before, &mut args, "--before")?,
            "--after" => push_sources(&mut after, &mut args, "--after")?,
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

    let before = before.ok_or_else(|| Error::Usage(format!("missing --before\n\n{}", usage())))?;
    let after = after.ok_or_else(|| Error::Usage(format!("missing --after\n\n{}", usage())))?;

    Ok(CheckConfig {
        before,
        after,
        import_paths,
    })
}

pub fn usage() -> String {
    "usage: captain check --before <path|glob>... --after <path|glob>... [-I <path>]...".to_owned()
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
