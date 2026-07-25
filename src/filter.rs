/// Parser and matcher for `.omemfs-filter` files.
///
/// Format:
///   Lines before the first section header belong to `[ignore]`.
///   `[ignore]`     section: gitignore-subset patterns; matched paths are excluded from push/scan.
///   `[aggregate]`  section: gitignore-subset patterns; matched directories are collapsed in `ls`.
///
/// Pattern syntax (gitignore subset):
///   blank / `# comment`  — ignored
///   `/pattern`           — anchored to the directory containing the file
///   `**/pattern`         — any depth
///   `pattern`            — equivalent to `**/pattern`
///   `*`                  — any string not containing `/`
///   trailing `/`         — optional, no effect
///   `!pattern`           — negates a previously matched pattern
///
/// Unsupported (silently skipped): `?`, `[abc]`.
use std::path::Path;

// ---------------------------------------------------------------------------
// Pattern
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
struct Pattern {
    /// The original text of the pattern (without leading `!`).
    text: String,
    /// True when the pattern starts with `/` (anchor to the file's directory).
    anchored: bool,
    /// True when the line was prefixed with `!` (negation).
    negated: bool,
}

impl Pattern {
    fn parse(line: &str) -> Option<Pattern> {
        let line = line.trim_end();
        if line.is_empty() || line.starts_with('#') {
            return None;
        }

        // Handle `\#` and `\!` escape sequences at line start.
        let (negated, rest) = if line.starts_with("\\#") || line.starts_with("\\!") {
            (false, &line[1..])
        } else if let Some(rest) = line.strip_prefix('!') {
            (true, rest)
        } else {
            (false, line)
        };

        // Strip optional trailing `/`.
        let rest = rest.trim_end_matches('/');
        if rest.is_empty() {
            return None;
        }

        // Skip unsupported patterns containing `?` or `[`.
        if rest.contains('?') || rest.contains('[') {
            return None;
        }

        let anchored = rest.starts_with('/');
        let text = if anchored {
            rest[1..].to_string()
        } else {
            rest.to_string()
        };

        Some(Pattern {
            text,
            anchored,
            negated,
        })
    }

    /// Returns true if `rel_path` (relative to the directory containing the filter file,
    /// forward-slash separated, no leading slash) matches this pattern.
    fn matches(&self, rel_path: &str) -> bool {
        if self.anchored {
            // `/pattern` → match only at the top of the scoped directory.
            glob_match(&self.text, rel_path)
        } else if self.text.starts_with("**/") {
            // `**/pattern` → any depth.
            let suffix = &self.text[3..];
            // Match at root level.
            if glob_match(suffix, rel_path) {
                return true;
            }
            // Match any descendant.
            let mut rest = rel_path;
            while let Some(pos) = rest.find('/') {
                rest = &rest[pos + 1..];
                if glob_match(suffix, rest) {
                    return true;
                }
            }
            false
        } else {
            // Plain `pattern` → equivalent to `**/pattern`: match at any depth.
            // Match at root level.
            if glob_match(&self.text, rel_path) {
                return true;
            }
            // Match as a descendant.
            let mut rest = rel_path;
            while let Some(pos) = rest.find('/') {
                rest = &rest[pos + 1..];
                if glob_match(&self.text, rest) {
                    return true;
                }
            }
            false
        }
    }
}

/// Match `pattern` (which may contain `*`) against `name`.
/// The `*` wildcard matches any sequence of characters that does not contain `/`.
fn glob_match(pattern: &str, name: &str) -> bool {
    if !pattern.contains('*') {
        return pattern == name;
    }
    let parts: Vec<&str> = pattern.split('*').collect();
    let n = parts.len();
    let mut remaining = name;

    for (i, part) in parts.iter().enumerate() {
        if i == 0 {
            if !remaining.starts_with(part) {
                return false;
            }
            remaining = &remaining[part.len()..];
        } else if i == n - 1 {
            // Last segment: must match the end, no `/` allowed in the matched region.
            if !remaining.ends_with(part) {
                return false;
            }
            let matched = &remaining[..remaining.len() - part.len()];
            if matched.contains('/') {
                return false;
            }
        } else {
            // Middle segment: find the next occurrence, no `/` allowed in skipped region.
            let pos = match remaining.find(part) {
                Some(p) => p,
                None => return false,
            };
            let skipped = &remaining[..pos];
            if skipped.contains('/') {
                return false;
            }
            remaining = &remaining[pos + part.len()..];
        }
    }
    true
}

// ---------------------------------------------------------------------------
// Section
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct Section {
    patterns: Vec<Pattern>,
}

impl Section {
    /// Returns true if `rel_path` is matched by this section's pattern list,
    /// honouring negation (`!pattern`).
    fn matches(&self, rel_path: &str) -> bool {
        let mut matched = false;
        for p in &self.patterns {
            if p.matches(rel_path) {
                matched = !p.negated;
            }
        }
        matched
    }
}

// ---------------------------------------------------------------------------
// FilterFile — one parsed `.omemfs-filter` file
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
struct FilterFile {
    ignore: Section,
    aggregate: Section,
}

impl FilterFile {
    fn parse(content: &str) -> FilterFile {
        let mut f = FilterFile::default();
        let mut current_is_aggregate = false;

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed == "[ignore]" {
                current_is_aggregate = false;
                continue;
            }
            if trimmed == "[aggregate]" {
                current_is_aggregate = true;
                continue;
            }
            if let Some(pat) = Pattern::parse(line) {
                if current_is_aggregate {
                    f.aggregate.patterns.push(pat);
                } else {
                    f.ignore.patterns.push(pat);
                }
            }
        }
        f
    }
}

// ---------------------------------------------------------------------------
// FilterSet — collection of FilterFile entries loaded from the working tree
// ---------------------------------------------------------------------------

/// A loaded set of `.omemfs-filter` files for a working tree.
///
/// Call `load` once from the repository root, then call `is_ignored` /
/// `is_aggregated` for individual paths during a scan.
#[derive(Debug, Default)]
pub struct FilterSet {
    /// Each entry: (directory path relative to work_dir, parsed FilterFile).
    /// Sorted so that the root-level file (empty prefix) comes first.
    files: Vec<(String, FilterFile)>,
}

impl FilterSet {
    /// Load all `.omemfs-filter` files found under `work_dir`.
    pub fn load(work_dir: &Path) -> FilterSet {
        let mut set = FilterSet::default();
        collect_filter_files(work_dir, work_dir, "", &mut set.files);
        set
    }

    /// Load only the `.omemfs-filter` files that can affect paths under
    /// `scope_prefix` (design/05 "Scope-limited filter load").
    ///
    /// `scope_prefix` is a repo-relative forward-slash path with no leading or
    /// trailing slash (e.g. `src` or `src/codec`). An empty prefix is identical
    /// to [`load`](Self::load).
    ///
    /// Loads the root filter and the filter in each ancestor directory along
    /// `scope_prefix` by direct stat (no sibling directories are visited), then
    /// walks only the scoped subtree for filters inside it. This is exactly the
    /// set of files whose subtree contains or is contained by `scope_prefix`, so
    /// `is_ignored` / `is_aggregated` for in-scope paths match a full load.
    pub fn load_scoped(work_dir: &Path, scope_prefix: &str) -> FilterSet {
        let scope_prefix = scope_prefix.trim_matches('/');
        if scope_prefix.is_empty() {
            return Self::load(work_dir);
        }

        let mut set = FilterSet::default();

        // Load the root filter and each ancestor directory's filter, top-down.
        // Stop early if an ancestor's `[ignore]` excludes the path leading to the
        // scope: a `.omemfs-filter` inside an ignored directory is never read
        // (same rule as the recursive walk).
        let components: Vec<&str> = scope_prefix.split('/').filter(|s| !s.is_empty()).collect();
        // Root (empty prefix) first.
        load_filter_file(work_dir, "", &mut set.files);
        let mut cur_prefix = String::new();
        let mut cur_dir = work_dir.to_path_buf();
        for comp in &components {
            cur_dir = cur_dir.join(comp);
            cur_prefix = if cur_prefix.is_empty() {
                (*comp).to_string()
            } else {
                format!("{}/{}", cur_prefix, comp)
            };
            // If this ancestor directory is ignored by an already-loaded filter,
            // the scope itself is inside an ignored subtree: stop loading.
            if is_ignored_by_loaded(&set.files, &cur_prefix) {
                return set;
            }
            load_filter_file(&cur_dir, &cur_prefix, &mut set.files);
        }

        // Walk only the scoped subtree for filters inside it. cur_dir now points
        // at the scope directory and cur_prefix == scope_prefix. The scope's own
        // filter was already loaded by the ancestor loop above, so descend into
        // its children only (avoid loading the scope's own file twice).
        if let Ok(rd) = std::fs::read_dir(&cur_dir) {
            for entry in rd.flatten() {
                let name = entry.file_name().to_string_lossy().into_owned();
                if name == ".omemfs" || name == ".omemfs-filter" {
                    continue;
                }
                let child = entry.path();
                if child.is_dir() {
                    let child_prefix = format!("{}/{}", cur_prefix, name);
                    if is_ignored_by_loaded(&set.files, &child_prefix) {
                        continue;
                    }
                    collect_filter_files(work_dir, &child, &child_prefix, &mut set.files);
                }
            }
        }
        set
    }

    /// Returns true if `rel_path` (forward-slash separated, relative to `work_dir`)
    /// is excluded from working tree scans by any `[ignore]` section.
    pub fn is_ignored(&self, rel_path: &str) -> bool {
        self.test(rel_path, |ff, path_in_scope| {
            ff.ignore.matches(path_in_scope)
        })
    }

    /// Returns true if `rel_path` is marked for aggregated display by any
    /// `[aggregate]` section.
    #[allow(dead_code)]
    pub fn is_aggregated(&self, rel_path: &str) -> bool {
        self.test(rel_path, |ff, path_in_scope| {
            ff.aggregate.matches(path_in_scope)
        })
    }

    fn test<F>(&self, rel_path: &str, check: F) -> bool
    where
        F: Fn(&FilterFile, &str) -> bool,
    {
        for (dir_prefix, ff) in &self.files {
            let path_in_scope = if dir_prefix.is_empty() {
                rel_path
            } else {
                let prefix_slash = format!("{}/", dir_prefix);
                match rel_path.strip_prefix(prefix_slash.as_str()) {
                    Some(s) => s,
                    None => continue,
                }
            };
            if check(ff, path_in_scope) {
                return true;
            }
        }
        false
    }
}

/// Load the `.omemfs-filter` in `dir` (if present) and append it to `out` keyed
/// by `rel_prefix`. Does not descend into subdirectories.
fn load_filter_file(dir: &Path, rel_prefix: &str, out: &mut Vec<(String, FilterFile)>) {
    let filter_path = dir.join(".omemfs-filter");
    if filter_path.is_file()
        && let Ok(content) = std::fs::read_to_string(&filter_path)
    {
        out.push((rel_prefix.to_string(), FilterFile::parse(&content)));
    }
}

fn collect_filter_files(
    _work_dir: &Path,
    dir: &Path,
    rel_prefix: &str,
    out: &mut Vec<(String, FilterFile)>,
) {
    load_filter_file(dir, rel_prefix, out);

    let rd = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in rd.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == ".omemfs" || name == ".omemfs-filter" {
            continue;
        }
        let child = entry.path();
        if child.is_dir() {
            let child_prefix = if rel_prefix.is_empty() {
                name.clone()
            } else {
                format!("{}/{}", rel_prefix, name)
            };
            // Do not descend into directories that are ignored by an
            // already-loaded `.omemfs-filter` (an ancestor or this directory's
            // own file). A `.omemfs-filter` inside an ignored directory has no
            // effect, so it must never be read (design/05 "Hierarchical
            // application": "When a directory is excluded by [ignore], the scan
            // does not descend into it, so .omemfs-filter files inside it are
            // never read"). Recursion is top-down, so every relevant ancestor
            // file is already present in `out` when this check runs.
            if is_ignored_by_loaded(out, &child_prefix) {
                continue;
            }
            collect_filter_files(_work_dir, &child, &child_prefix, out);
        }
    }
}

/// Returns true if `rel_path` is ignored by any of the `[ignore]` sections in
/// the already-loaded filter files. Mirrors `FilterSet::is_ignored` but operates
/// on a partially-built file list during collection.
fn is_ignored_by_loaded(files: &[(String, FilterFile)], rel_path: &str) -> bool {
    for (dir_prefix, ff) in files {
        let path_in_scope = if dir_prefix.is_empty() {
            rel_path
        } else {
            let prefix_slash = format!("{}/", dir_prefix);
            match rel_path.strip_prefix(prefix_slash.as_str()) {
                Some(s) => s,
                None => continue,
            }
        };
        if ff.ignore.matches(path_in_scope) {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Default template content
// ---------------------------------------------------------------------------

pub const DEFAULT_FILTER_TEMPLATE: &str = r#"# omemfs filter configuration
# Generated by `omemfs clone`. You may freely edit, remove, or add patterns.
# This file is tracked in omemfs and synced alongside other files.

[ignore]
# Paths matching these patterns are excluded from push/scan.
# Syntax is the same as .gitignore (including ! for negation).

# Microsoft Office lock files
~$*

# LibreOffice lock files
.~lock.*#

# macOS metadata
.DS_Store

# Windows metadata
Thumbs.db
desktop.ini

# Build artifacts
target/
node_modules/
__pycache__/

[aggregate]
# Directories matching these patterns are shown as a single entry in `omemfs ls`.
# They are still synced to the remote normally.

# Version control metadata
.git

# Build artifacts (large trees; shown collapsed even though not ignored)
target/
node_modules/
__pycache__/
"#;

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn matches_ignore(content: &str, path: &str) -> bool {
        FilterFile::parse(content).ignore.matches(path)
    }

    #[allow(dead_code)]
    fn matches_aggregate(content: &str, path: &str) -> bool {
        FilterFile::parse(content).aggregate.matches(path)
    }

    // --- Pattern parsing ---

    #[test]
    fn empty_file_matches_nothing() {
        assert!(!matches_ignore("", "anything"));
    }

    #[test]
    fn comment_lines_ignored() {
        assert!(!matches_ignore("# target/\n", "target"));
    }

    #[test]
    fn blank_lines_ignored() {
        assert!(!matches_ignore("\n\n\n", "x"));
    }

    // --- Basic matching (implicit [ignore] section) ---

    #[test]
    fn plain_pattern_matches_at_root() {
        assert!(matches_ignore("target/\n", "target"));
    }

    #[test]
    fn plain_pattern_matches_nested() {
        assert!(matches_ignore("target/\n", "sub/target"));
        assert!(matches_ignore("target/\n", "a/b/target"));
    }

    #[test]
    fn anchored_pattern_matches_root_only() {
        assert!(matches_ignore("/target\n", "target"));
        assert!(!matches_ignore("/target\n", "sub/target"));
    }

    #[test]
    fn double_star_pattern_matches_any_depth() {
        assert!(matches_ignore("**/node_modules\n", "node_modules"));
        assert!(matches_ignore("**/node_modules\n", "sub/node_modules"));
        assert!(matches_ignore("**/node_modules\n", "a/b/node_modules"));
    }

    #[test]
    fn glob_star_matches_extension() {
        assert!(matches_ignore("**/*.pyc\n", "foo.pyc"));
        assert!(matches_ignore("**/*.pyc\n", "sub/foo.pyc"));
        assert!(!matches_ignore("**/*.pyc\n", "sub/foo.py"));
    }

    #[test]
    fn glob_star_matches_prefix() {
        assert!(matches_ignore("~$*\n", "~$document.docx"));
        assert!(!matches_ignore("~$*\n", "document.docx"));
    }

    // --- Negation ---

    #[test]
    fn negation_unmatches() {
        let content = "logs/\n!logs/keep.log\n";
        assert!(matches_ignore(content, "logs"));
        assert!(!matches_ignore(content, "logs/keep.log"));
    }

    // --- Unsupported syntax silently skipped ---

    #[test]
    fn question_mark_skipped() {
        // `foo?ar` would be skipped; file should not panic and pattern is ignored.
        assert!(!matches_ignore("foo?ar\n", "foobar"));
    }

    #[test]
    fn character_class_skipped() {
        assert!(!matches_ignore("foo[ab]\n", "fooa"));
    }

    // --- Sections ---

    #[test]
    fn no_section_header_is_ignore() {
        let content = "target/\n";
        let f = FilterFile::parse(content);
        assert!(f.ignore.matches("target"));
        assert!(!f.aggregate.matches("target"));
    }

    #[test]
    fn explicit_ignore_section() {
        let content = "[ignore]\ntarget/\n";
        let f = FilterFile::parse(content);
        assert!(f.ignore.matches("target"));
        assert!(!f.aggregate.matches("target"));
    }

    #[test]
    fn aggregate_section() {
        let content = "[aggregate]\n.git\n";
        let f = FilterFile::parse(content);
        assert!(!f.ignore.matches(".git"));
        assert!(f.aggregate.matches(".git"));
    }

    #[test]
    fn both_sections() {
        let content = "[ignore]\ntarget/\n[aggregate]\ntarget/\n.git\n";
        let f = FilterFile::parse(content);
        assert!(f.ignore.matches("target"));
        assert!(f.aggregate.matches("target"));
        assert!(!f.ignore.matches(".git"));
        assert!(f.aggregate.matches(".git"));
    }

    #[test]
    fn lines_before_section_header_are_ignore() {
        let content = "target/\n[aggregate]\n.git\n";
        let f = FilterFile::parse(content);
        assert!(f.ignore.matches("target"));
        assert!(!f.aggregate.matches("target"));
        assert!(f.aggregate.matches(".git"));
    }

    // --- FilterSet ---

    #[test]
    fn filter_set_empty_dir_matches_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let set = FilterSet::load(dir.path());
        assert!(!set.is_ignored("target"));
        assert!(!set.is_aggregated(".git"));
    }

    #[test]
    fn filter_set_root_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".omemfs-filter"),
            "[ignore]\ntarget/\n[aggregate]\n.git\n",
        )
        .unwrap();
        let set = FilterSet::load(dir.path());
        assert!(set.is_ignored("target"));
        assert!(set.is_ignored("sub/target"));
        assert!(set.is_aggregated(".git"));
        assert!(!set.is_ignored(".git"));
    }

    #[test]
    fn filter_set_does_not_read_filter_inside_ignored_dir() {
        // A `.omemfs-filter` inside an ignored directory must have no effect:
        // collection must not descend into ignored directories.
        let dir = tempfile::tempdir().unwrap();
        // Root ignores the directory `build/`.
        std::fs::write(dir.path().join(".omemfs-filter"), "[ignore]\nbuild/\n").unwrap();
        std::fs::create_dir(dir.path().join("build")).unwrap();
        // A filter inside the ignored directory declares an aggregate rule.
        // If this file is (wrongly) loaded, `is_aggregated("build/sub")` would
        // become true. Because the directory is ignored, the file must not be
        // read at all, so the rule must have no effect.
        std::fs::write(
            dir.path().join("build/.omemfs-filter"),
            "[aggregate]\nsub\n",
        )
        .unwrap();
        let set = FilterSet::load(dir.path());
        assert!(set.is_ignored("build"));
        // The nested filter inside the ignored directory must not be loaded.
        assert!(!set.is_aggregated("build/sub"));
    }

    #[test]
    fn filter_set_subdirectory_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub/.omemfs-filter"), "[ignore]\n/dist\n").unwrap();
        let set = FilterSet::load(dir.path());
        // `sub/dist` should be ignored (anchored `/dist` in `sub/.omemfs-filter`)
        assert!(set.is_ignored("sub/dist"));
        // `dist` at root should NOT be ignored
        assert!(!set.is_ignored("dist"));
        // `other/dist` should NOT be ignored
        assert!(!set.is_ignored("other/dist"));
    }

    // --- FilterSet::load_scoped (design/05 "Scope-limited filter load") ---

    #[test]
    fn load_scoped_applies_ancestor_filter() {
        // A root filter ignoring `target/` must still apply to in-scope paths
        // when loading scope-limited for a subtree.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".omemfs-filter"), "[ignore]\ntarget/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src/sub")).unwrap();
        let set = FilterSet::load_scoped(dir.path(), "src");
        // The ancestor (root) rule applies to in-scope descendants.
        assert!(set.is_ignored("src/target"));
        assert!(set.is_ignored("src/sub/target"));
    }

    #[test]
    fn load_scoped_applies_in_scope_filter() {
        // A filter inside the scoped subtree must be loaded and applied.
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/.omemfs-filter"), "[ignore]\n/dist\n").unwrap();
        let set = FilterSet::load_scoped(dir.path(), "src");
        assert!(set.is_ignored("src/dist"));
    }

    #[test]
    fn load_scoped_matches_full_load_for_in_scope_paths() {
        // For any in-scope path, load_scoped must agree with the full load.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".omemfs-filter"), "[ignore]\ntarget/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src/codec")).unwrap();
        std::fs::write(
            dir.path().join("src/.omemfs-filter"),
            "[ignore]\n/generated\n",
        )
        .unwrap();
        // An out-of-scope sibling filter that must NOT influence in-scope checks
        // (and must not even be read).
        std::fs::create_dir_all(dir.path().join("other")).unwrap();
        std::fs::write(dir.path().join("other/.omemfs-filter"), "[ignore]\n/x\n").unwrap();

        let full = FilterSet::load(dir.path());
        let scoped = FilterSet::load_scoped(dir.path(), "src");
        for path in [
            "src/target",
            "src/generated",
            "src/codec/target",
            "src/keep.rs",
        ] {
            assert_eq!(
                full.is_ignored(path),
                scoped.is_ignored(path),
                "mismatch for in-scope path {}",
                path
            );
        }
    }

    #[test]
    fn load_scoped_does_not_descend_into_ignored_dir() {
        // Mirror of filter_set_does_not_read_filter_inside_ignored_dir for the
        // scoped loader: a filter inside an ignored in-scope directory is skipped.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".omemfs-filter"), "[ignore]\nsrc/build/\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src/build")).unwrap();
        std::fs::write(
            dir.path().join("src/build/.omemfs-filter"),
            "[aggregate]\nsub\n",
        )
        .unwrap();
        let set = FilterSet::load_scoped(dir.path(), "src");
        assert!(set.is_ignored("src/build"));
        assert!(!set.is_aggregated("src/build/sub"));
    }

    #[test]
    fn load_scoped_empty_prefix_equals_full_load() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(".omemfs-filter"), "[ignore]\ntarget/\n").unwrap();
        let scoped = FilterSet::load_scoped(dir.path(), "");
        assert!(scoped.is_ignored("target"));
        assert!(scoped.is_ignored("a/b/target"));
    }
}
