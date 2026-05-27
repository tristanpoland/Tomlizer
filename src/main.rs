use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use owo_colors::OwoColorize;
use spinners::{Spinner, Spinners};
use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use toml_edit::{Document, DocumentMut, InlineTable, Item, Table, Value};
use walkdir::WalkDir;

const IGNORED_DIRS: &[&str] = &["target", ".git", "node_modules", "tools"];
const STRIPPED_KEYS: &[&str] = &["path", "version", "git", "branch", "tag", "rev", "registry", "default-features"];
const ROOT_DEP_KEYS: &[&str] = &["version", "git", "branch", "tag", "rev", "registry", "package", "features", "default-features", "optional", "default-features"];

#[derive(Parser)]
#[command(author, version, about = "Scan Cargo.toml files and normalize repeated dependencies to workspace references.", long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,

    /// Root directory to scan.
    #[arg(long, default_value = ".")]
    root: PathBuf,
}

#[derive(Subcommand)]
enum Command {
    /// Print suggestions for workspace dependency normalization.
    Scan,
    /// Apply suggestions to the root workspace Cargo.toml and child manifests.
    Apply {
        /// Skip confirmation prompt and apply changes immediately.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Debug, Clone)]
struct DepUsage {
    manifest: PathBuf,
    section_path: Vec<String>,
    name: String,
    item: Item,
    is_workspace: bool,
}

#[derive(Debug)]
struct ChildFix {
    manifest: PathBuf,
    section_path: Vec<String>,
    name: String,
    new_item: Item,
}

#[derive(Debug)]
struct RootAddition {
    name: String,
    item: Item,
}

#[derive(Debug, Default)]
struct FixPlan {
    root_additions: Vec<RootAddition>,
    child_fixes: Vec<ChildFix>,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let root = cli
        .root
        .canonicalize()
        .context("Failed to canonicalize root path")?;
    let root_manifest = root.join("Cargo.toml");
    if !root_manifest.exists() {
        bail!("Root Cargo.toml not found at {}", root_manifest.display());
    }

    let mut spinner = Spinner::new(Spinners::Dots9, "Scanning Cargo.toml files...".into());
    let manifests = collect_workspace_manifests(&root)?;
    let root_doc = load_document(&root_manifest)?;
    let local_packages = collect_local_packages(&manifests, &root_manifest)?;
    let workspace_deps = collect_workspace_dependencies(&root_doc);
    let dep_usages = collect_dependency_usages(&manifests, &root_manifest)?;
    let fixes = analyze_fixes(&dep_usages, &workspace_deps, &local_packages, &root);
    spinner.stop();

    print_summary(&fixes);

    match cli.command {
        Command::Scan => Ok(()),
        Command::Apply { yes } => {
            if fixes.root_additions.is_empty() && fixes.child_fixes.is_empty() {
                println!("Nothing to apply. Workspace dependencies are already normalized.");
                return Ok(());
            }

            if !yes {
                prompt_apply()?;
            }

            let mut apply_spinner = Spinner::new(Spinners::Dots9, "Applying fixes...".into());
            apply_fixes(&root_manifest, fixes)?;
            apply_spinner.stop();
            println!("{}", "Applied workspace fixes.".green());
            Ok(())
        }
    }
}

fn collect_workspace_manifests(root: &Path) -> Result<Vec<PathBuf>> {
    let mut manifests = Vec::new();
    for entry in WalkDir::new(root).follow_links(false) {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type().is_dir() && IGNORED_DIRS.iter().any(|ignored| path.ends_with(ignored)) {
            continue;
        }
        if entry.file_type().is_file() && path.file_name().map(|name| name == "Cargo.toml").unwrap_or(false)
        {
            manifests.push(path.to_path_buf());
        }
    }
    manifests.sort();
    Ok(manifests)
}

fn load_document(path: &Path) -> Result<Document<String>> {
    let content = fs::read_to_string(path).with_context(|| format!("Failed to read {}", path.display()))?;
    content
        .parse::<Document<String>>()
        .with_context(|| format!("Failed to parse {}", path.display()))
}

fn collect_local_packages(manifests: &[PathBuf], root_manifest: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut packages = BTreeMap::new();
    for manifest in manifests {
        if manifest == root_manifest {
            continue;
        }
        let doc = load_document(manifest)?;
        if let Some(name) = doc
            .get("package")
            .and_then(Item::as_table)
            .and_then(|package| package.get("name"))
            .and_then(Item::as_str)
        {
            packages.insert(name.to_string(), manifest.clone());
        }
    }
    Ok(packages)
}

fn collect_workspace_dependencies(root_doc: &Document<String>) -> BTreeMap<String, Item> {
    let mut dependencies = BTreeMap::new();
    if let Some(table) = root_doc
        .get("workspace")
        .and_then(Item::as_table)
        .and_then(|workspace| workspace.get("dependencies"))
        .and_then(Item::as_table)
    {
        for (key, item) in table.iter() {
            dependencies.insert(key.to_string(), item.clone());
        }
    }
    dependencies
}

fn collect_dependency_usages(manifests: &[PathBuf], root_manifest: &Path) -> Result<HashMap<String, Vec<DepUsage>>> {
    let mut usage_map = HashMap::new();
    for manifest in manifests {
        if manifest == root_manifest {
            continue;
        }
        let doc = load_document(manifest)?;
        collect_dependencies_in_document(&doc, manifest, &mut usage_map);
    }
    Ok(usage_map)
}

fn collect_dependencies_in_document(doc: &Document<String>, manifest: &Path, usage_map: &mut HashMap<String, Vec<DepUsage>>) {
    let top_sections = ["dependencies", "dev-dependencies", "build-dependencies"];
    for section in top_sections {
        if let Some(table) = doc.get(section).and_then(Item::as_table) {
            collect_dependencies_in_table(table, manifest, vec![section.to_string()], usage_map);
        }
    }

    if let Some(target_table) = doc.get("target").and_then(Item::as_table) {
        for (target_name, target_item) in target_table.iter() {
            if let Some(target_section) = target_item.as_table() {
                for section in top_sections {
                    if let Some(target_deps) = target_section.get(section).and_then(Item::as_table) {
                        let path = vec![
                            "target".to_string(),
                            target_name.to_string(),
                            section.to_string(),
                        ];
                        collect_dependencies_in_table(target_deps, manifest, path, usage_map);
                    }
                }
            }
        }
    }
}

fn collect_dependencies_in_table(
    table: &Table,
    manifest: &Path,
    section_path: Vec<String>,
    usage_map: &mut HashMap<String, Vec<DepUsage>>,
) {
    for (dep_name, dep_item) in table.iter() {
        let usage = DepUsage {
            manifest: manifest.to_path_buf(),
            section_path: section_path.clone(),
            name: dep_name.to_string(),
            item: dep_item.clone(),
            is_workspace: is_workspace_dependency(dep_item),
        };
        usage_map.entry(usage.name.clone()).or_default().push(usage);
    }
}

fn is_workspace_dependency(item: &Item) -> bool {
    if let Some(table) = item.as_table() {
        return get_bool_key(table, "workspace").unwrap_or(false);
    }
    if let Some(value) = item.as_value() {
        if let Some(inline) = value.as_inline_table() {
            return get_bool_key(inline, "workspace").unwrap_or(false);
        }
    }
    false
}

fn item_has_key(item: &Item, key: &str) -> bool {
    if let Some(table) = item.as_table() {
        return table.contains_key(key);
    }
    if let Some(value) = item.as_value() {
        if let Some(inline) = value.as_inline_table() {
            return inline.get(key).is_some();
        }
    }
    false
}

fn get_bool_key<T: ?Sized + toml_edit::TableLike>(table: &T, key: &str) -> Option<bool> {
    table.get(key).and_then(|item| item.as_value()).and_then(|value| value.as_bool())
}

fn item_has_version(item: &Item) -> bool {
    if let Some(value) = item.as_value() {
        if value.as_str().is_some() {
            return true;
        }
        if let Some(inline) = value.as_inline_table() {
            return inline.get("version").is_some();
        }
    }
    if let Some(table) = item.as_table() {
        return table.contains_key("version");
    }
    false
}

fn analyze_fixes(
    dep_usages: &HashMap<String, Vec<DepUsage>>,
    root_workspace_deps: &BTreeMap<String, Item>,
    local_packages: &BTreeMap<String, PathBuf>,
    root_dir: &Path,
) -> FixPlan {
    let mut plan = FixPlan::default();

    for (name, usages) in dep_usages {
        let root_workspace_dep = root_workspace_deps.get(name);
        let has_root_workspace_dep = root_workspace_dep.is_some();
        let local_manifest = local_packages.get(name);
        if usages.len() < 2 {
            if !has_root_workspace_dep && usages.iter().any(|usage| usage.is_workspace) {
                if let Some(local_manifest) = local_manifest {
                    push_root_addition(
                        &mut plan,
                        name,
                        build_local_workspace_root_entry(root_dir, local_manifest),
                    );
                }
            }
            continue;
        }

        let non_workspace_usages: Vec<_> = usages.iter().filter(|usage| !usage.is_workspace).collect();
        if non_workspace_usages.is_empty() {
            if !has_root_workspace_dep {
                if let Some(local_manifest) = local_manifest {
                    push_root_addition(
                        &mut plan,
                        name,
                        build_local_workspace_root_entry(root_dir, local_manifest),
                    );
                }
            }
            continue;
        }

        if !has_root_workspace_dep {
            if let Some(local_manifest) = local_manifest {
                push_root_addition(
                    &mut plan,
                    name,
                    build_local_workspace_root_entry(root_dir, local_manifest),
                );

                for usage in &non_workspace_usages {
                    let new_item = build_workspace_child_item(usage);
                    plan.child_fixes.push(ChildFix {
                        manifest: usage.manifest.clone(),
                        section_path: usage.section_path.clone(),
                        name: usage.name.clone(),
                        new_item,
                    });
                }
                continue;
            }
        }

        if has_root_workspace_dep {
            let root_workspace_dep = root_workspace_dep.expect("workspace dependency should exist");
            for usage in &non_workspace_usages {
                if !can_inherit_from_workspace_dep(usage, root_workspace_dep) {
                    continue;
                }
                let new_item = build_workspace_child_item(usage);
                plan.child_fixes.push(ChildFix {
                    manifest: usage.manifest.clone(),
                    section_path: usage.section_path.clone(),
                    name: usage.name.clone(),
                    new_item,
                });
            }
            continue;
        }

        if !non_workspace_usages
            .iter()
            .all(|usage| can_share_workspace_dependency(usage, non_workspace_usages[0]))
        {
            continue;
        }

        if let Some(root_item) = build_root_workspace_child_entry(non_workspace_usages[0]) {
            plan.root_additions.push(RootAddition {
                name: name.clone(),
                item: root_item,
            });
            for usage in &non_workspace_usages {
                let new_item = build_workspace_child_item(usage);
                plan.child_fixes.push(ChildFix {
                    manifest: usage.manifest.clone(),
                    section_path: usage.section_path.clone(),
                    name: usage.name.clone(),
                    new_item,
                });
            }
        }
    }

    plan
}

fn can_inherit_from_workspace_dep(usage: &DepUsage, root_item: &Item) -> bool {
    if let Some(default_features) = item_default_features(&usage.item) {
        return default_features == item_effective_default_features(root_item);
    }

    true
}

fn can_share_workspace_dependency(left: &DepUsage, right: &DepUsage) -> bool {
    item_default_features(&left.item) == item_default_features(&right.item)
}

fn item_default_features(item: &Item) -> Option<bool> {
    if !item_has_key(item, "default-features") {
        return None;
    }

    if let Some(table) = item.as_table() {
        return get_bool_key(table, "default-features");
    }
    if let Some(value) = item.as_value() {
        if let Some(inline) = value.as_inline_table() {
            return get_bool_key(inline, "default-features");
        }
    }

    None
}

fn item_effective_default_features(item: &Item) -> bool {
    item_default_features(item).unwrap_or(true)
}

fn push_root_addition(plan: &mut FixPlan, name: &str, item: Item) {
    if plan.root_additions.iter().any(|addition| addition.name == name) {
        return;
    }

    plan.root_additions.push(RootAddition {
        name: name.to_string(),
        item,
    });
}

fn build_local_workspace_root_entry(root_dir: &Path, manifest: &Path) -> Item {
    let local_dir = manifest.parent().unwrap_or(root_dir);
    let relative_path = local_dir.strip_prefix(root_dir).unwrap_or(local_dir);

    let mut inline = InlineTable::new();
    inline.insert("path", Value::from(relative_path.to_string_lossy().to_string()));
    inline.fmt();

    Item::Value(Value::InlineTable(inline))
}

fn build_root_workspace_child_entry(usage: &DepUsage) -> Option<Item> {
    if let Some(value) = usage.item.as_value().and_then(Value::as_str) {
        return Some(Item::Value(Value::from(value.to_string())));
    }

    let mut inline = InlineTable::new();
    for (key, item) in dependency_item_pairs(&usage.item) {
        if ROOT_DEP_KEYS.contains(&key.as_str()) {
            if let Ok(value) = item.clone().into_value() {
                inline.insert(key, value);
            }
        }
    }

    if inline.is_empty() {
        None
    } else {
        inline.fmt();
        Some(Item::Value(Value::InlineTable(inline)))
    }
}

fn build_workspace_child_item(usage: &DepUsage) -> Item {
    let mut inline = InlineTable::new();
    inline.insert("workspace", Value::from(true));

    for (key, item) in dependency_item_pairs(&usage.item) {
        if STRIPPED_KEYS.contains(&key.as_str()) {
            continue;
        }
        if key == "workspace" {
            continue;
        }
        if let Ok(value) = item.clone().into_value() {
            inline.insert(key, value);
        }
    }

    inline.fmt();
    Item::Value(Value::InlineTable(inline))
}

fn dependency_item_pairs(item: &Item) -> Vec<(String, Item)> {
    if let Some(table) = item.as_table() {
        return table
            .iter()
            .map(|(key, item)| (key.to_string(), item.clone()))
            .collect();
    }
    if let Some(value) = item.as_value() {
        if let Some(inline) = value.as_inline_table() {
            return inline
                .iter()
                .map(|(key, value)| (key.to_string(), Item::Value(value.clone())))
                .collect();
        }
    }
    Vec::new()
}

fn print_summary(plan: &FixPlan) {
    if plan.root_additions.is_empty() && plan.child_fixes.is_empty() {
        println!("{}", "No repeated dependency patterns found that should use workspace refs.".green());
        return;
    }

    if !plan.root_additions.is_empty() {
        println!("{}", "Suggested parent workspace dependency additions:".yellow().bold());
        for addition in &plan.root_additions {
            println!("  {} -> {}", addition.name.blue(), addition.item.to_string().trim());
        }
        println!();
    }

    if !plan.child_fixes.is_empty() {
        println!("{}", "Suggested child dependency updates:".yellow().bold());
        let mut by_manifest: BTreeMap<_, Vec<_>> = BTreeMap::new();
        for fix in &plan.child_fixes {
            by_manifest
                .entry(fix.manifest.clone())
                .or_default()
                .push(fix);
        }
        for (manifest, fixes) in by_manifest {
            println!("  {}", manifest.display().cyan());
            for fix in fixes {
                println!(
                    "    {} in {} => {}",
                    fix.name.magenta(),
                    fix.section_path.join("."),
                    fix.new_item.to_string().trim()
                );
            }
        }
        println!();
    }

    println!(
        "{}",
        "Run `cargo run --manifest-path tools/tomlizer/Cargo.toml -- apply` to apply these changes.".dimmed()
    );
}

fn prompt_apply() -> Result<()> {
    print!("Apply suggested workspace changes? [y/N] ");
    io::stdout().flush()?;
    let mut buffer = String::new();
    io::stdin().read_line(&mut buffer)?;
    let accepted = matches!(buffer.trim().to_lowercase().as_str(), "y" | "yes");
    if !accepted {
        bail!("Apply aborted.");
    }
    Ok(())
}

fn apply_fixes(root_manifest: &Path, plan: FixPlan) -> Result<()> {
    let mut root_doc = load_document(root_manifest)?.into_mut();
    if !root_doc.as_table().contains_key("workspace") {
        root_doc["workspace"] = Item::Table(Table::new());
    }

    let workspace = root_doc
        .as_table_mut()
        .get_mut("workspace")
        .context("Failed to access workspace table")?;
    let workspace_table = workspace
        .as_table_mut()
        .context("Root workspace entry is not a table")?;

    if !workspace_table.contains_key("dependencies") {
        workspace_table["dependencies"] = Item::Table(Table::new());
    }

    let root_table = workspace_table
        .get_mut("dependencies")
        .and_then(Item::as_table_mut)
        .context("Failed to create workspace.dependencies table")?;

    for addition in &plan.root_additions {
        if root_table.contains_key(&addition.name) {
            continue;
        }
        root_table[&addition.name] = addition.item.clone();
    }

    if !plan.root_additions.is_empty() {
        fs::write(root_manifest, root_doc.to_string())
            .with_context(|| format!("Failed to write {}", root_manifest.display()))?;
    }

    let mut grouped: BTreeMap<PathBuf, Vec<&ChildFix>> = BTreeMap::new();
    for fix in &plan.child_fixes {
        grouped.entry(fix.manifest.clone()).or_default().push(fix);
    }

    for (manifest, fixes) in grouped {
        let mut doc = load_document(&manifest)?.into_mut();
        for fix in fixes {
            set_dependency_item(&mut doc, &fix.section_path, &fix.name, fix.new_item.clone())?;
        }
        fs::write(&manifest, doc.to_string())
            .with_context(|| format!("Failed to write {}", manifest.display()))?;
    }

    Ok(())
}

fn set_dependency_item(doc: &mut DocumentMut, section_path: &[String], dep_name: &str, new_item: Item) -> Result<()> {
    if section_path.is_empty() {
        bail!("Empty section path when setting dependency {}", dep_name);
    }

    let mut current: &mut Item = doc.as_item_mut();
    for segment in section_path {
        current = current
            .as_table_mut()
            .map(|table: &mut Table| table.entry(segment).or_insert(Item::Table(Table::new())))
            .context("Failed to navigate to dependency section")?;
    }
    current.as_table_mut().context("Dependency section is not a table")?[dep_name] = new_item;
    Ok(())
}
