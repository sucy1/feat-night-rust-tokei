use std::{collections::BTreeMap, path::Path};

use ignore::{DirEntry, WalkBuilder, WalkState::Continue};

use rayon::prelude::*;

use crate::{
    config::Config,
    language::{Language, LanguageType},
};

const IGNORE_FILE: &str = ".tokeignore";

fn wildcard_match(pattern: &str, text: &str) -> bool {
    let p = pattern.chars().collect::<Vec<_>>();
    let t = text.chars().collect::<Vec<_>>();
    let pn = p.len();
    let tn = t.len();

    let mut dp = vec![vec![false; tn + 1]; pn + 1];
    dp[0][0] = true;

    for i in 1..=pn {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        } else {
            break;
        }
    }

    for i in 1..=pn {
        for j in 1..=tn {
            if p[i - 1] == '*' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[i - 1] == '?' || p[i - 1].eq_ignore_ascii_case(&t[j - 1]) {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }

    dp[pn][tn]
}

struct ExcludeMatcher {
    component_patterns: Vec<String>,
    path_patterns: Vec<String>,
}

impl ExcludeMatcher {
    fn new(patterns: &[&str]) -> Self {
        let mut component_patterns = Vec::new();
        let mut path_patterns = Vec::new();

        for pattern in patterns {
            if pattern.contains('/') {
                path_patterns.push(pattern.trim_start_matches('/').to_string());
            } else {
                component_patterns.push(pattern.to_string());
            }
        }

        Self {
            component_patterns,
            path_patterns,
        }
    }

    fn is_excluded(&self, path: &Path) -> bool {
        for component in path.components() {
            let comp_str = component.as_os_str().to_string_lossy();
            for pattern in &self.component_patterns {
                if wildcard_match(pattern, &comp_str) {
                    return true;
                }
            }
        }

        let path_str = path.to_string_lossy();
        for pattern in &self.path_patterns {
            if wildcard_match(pattern, &path_str) {
                return true;
            }
        }

        false
    }
}

pub fn get_all_files<A: AsRef<Path>>(
    paths: &[A],
    ignored_directories: &[&str],
    languages: &mut BTreeMap<LanguageType, Language>,
    config: &Config,
) {
    let languages = parking_lot::Mutex::new(languages);
    let (tx, rx) = crossbeam_channel::unbounded();

    let mut paths = paths.iter();
    let mut walker = WalkBuilder::new(paths.next().unwrap());

    for path in paths {
        walker.add(path);
    }

    if !ignored_directories.is_empty() {
        let matcher = ExcludeMatcher::new(ignored_directories);
        walker.filter_entry(move |entry| !matcher.is_excluded(entry.path()));
    }

    let ignore = config.no_ignore.map(|b| !b).unwrap_or(true);
    let ignore_dot = ignore && config.no_ignore_dot.map(|b| !b).unwrap_or(true);
    let ignore_vcs = ignore && config.no_ignore_vcs.map(|b| !b).unwrap_or(true);

    // Custom ignore files always work even if the `ignore` option is false,
    // so we only add if that option is not present.
    if ignore_dot {
        walker.add_custom_ignore_filename(IGNORE_FILE);
    }

    walker
        .git_exclude(ignore_vcs)
        .git_global(ignore_vcs)
        .git_ignore(ignore_vcs)
        .hidden(config.hidden.map(|b| !b).unwrap_or(true))
        .ignore(ignore_dot)
        .parents(ignore && config.no_ignore_parent.map(|b| !b).unwrap_or(true));

    walker.build_parallel().run(move || {
        let tx = tx.clone();
        Box::new(move |entry| {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    use ignore::Error;
                    if let Error::WithDepth { err: ref error, .. } = error {
                        if let Error::WithPath {
                            ref path,
                            err: ref error,
                        } = **error
                        {
                            error!("{} reading {}", error, path.display());
                            return Continue;
                        }
                    }
                    error!("{}", error);
                    return Continue;
                }
            };

            if entry.file_type().map_or(false, |ft| ft.is_file()) {
                tx.send(entry).unwrap();
            }

            Continue
        })
    });

    let rx_iter = rx
        .into_iter()
        .par_bridge()
        .filter_map(|e| LanguageType::from_path(e.path(), config).map(|l| (e, l)));

    let process = |(entry, language): (DirEntry, LanguageType)| {
        let result = language.parse(entry.into_path(), config);
        let mut lock = languages.lock();
        let entry = lock.entry(language).or_insert_with(Language::new);
        match result {
            Ok(stats) => {
                let func = config.for_each_fn;
                if let Some(f) = func {
                    f(language, stats.clone())
                };
                entry.add_report(stats)
            }
            Err((error, path)) => {
                entry.mark_inaccurate();
                error!("Error reading {}:\n{}", path.display(), error);
            }
        }
    };

    if let Some(types) = config.types.as_deref() {
        rx_iter.filter(|(_, l)| types.contains(l)).for_each(process)
    } else {
        rx_iter.for_each(process)
    }
}

pub(crate) fn get_extension(path: &Path) -> Option<String> {
    path.extension().map(|e| e.to_string_lossy().to_lowercase())
}

pub(crate) fn get_filename(path: &Path) -> Option<String> {
    path.file_name().map(|e| e.to_string_lossy().to_lowercase())
}

#[cfg(test)]
mod tests {
    use std::fs;

    use tempfile::TempDir;

    use super::IGNORE_FILE;
    use super::wildcard_match;
    use crate::{
        config::Config,
        language::{languages::Languages, LanguageType},
    };

    const FILE_CONTENTS: &[u8] = b"fn main() {}";
    const FILE_NAME: &str = "main.rs";
    const IGNORE_PATTERN: &str = "*.rs";
    const LANGUAGE: &LanguageType = &LanguageType::Rust;

    #[test]
    fn wildcard_match_exact() {
        assert!(wildcard_match("vendor", "vendor"));
        assert!(!wildcard_match("vendor", "vendors"));
    }

    #[test]
    fn wildcard_match_star() {
        assert!(wildcard_match("test_*", "test_foo"));
        assert!(wildcard_match("test_*", "test_"));
        assert!(wildcard_match("*_test", "foo_test"));
        assert!(wildcard_match("*", "anything"));
        assert!(!wildcard_match("test_*", "prod_foo"));
    }

    #[test]
    fn wildcard_match_question() {
        assert!(wildcard_match("test?", "test1"));
        assert!(!wildcard_match("test?", "test"));
        assert!(!wildcard_match("test?", "test12"));
    }

    #[test]
    fn wildcard_match_case_insensitive() {
        assert!(wildcard_match("vendor", "Vendor"));
        assert!(wildcard_match("TEST_*", "test_foo"));
    }

    #[test]
    fn wildcard_match_path() {
        assert!(wildcard_match("*/vendor", "src/vendor"));
        assert!(wildcard_match("*/vendor", "lib/vendor"));
        assert!(!wildcard_match("*/vendor", "src/lib"));
    }

    #[test]
    fn ignore_directory_with_extension() {
        let mut languages = Languages::new();
        let tmp_dir = TempDir::new().expect("Couldn't create temp dir");
        let path_name = tmp_dir.path().join("directory.rs");

        fs::create_dir(path_name).expect("Couldn't create directory.rs within temp");

        super::get_all_files(
            &[tmp_dir.into_path().to_str().unwrap()],
            &[],
            &mut languages,
            &Config::default(),
        );

        assert!(languages.get(LANGUAGE).is_none());
    }

    #[test]
    fn hidden() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        fs::write(dir.path().join(".hidden.rs"), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.hidden = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn no_ignore_implies_dot() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        fs::write(dir.path().join(".ignore"), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.no_ignore = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn no_ignore_implies_vcs_gitignore() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        git2::Repository::init(dir.path()).expect("Couldn't create git repo.");

        fs::write(dir.path().join(".gitignore"), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.no_ignore = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn no_ignore_parent() {
        let parent_dir = TempDir::new().expect("Couldn't create temp dir.");
        let child_dir = parent_dir.path().join("child/");
        let mut config = Config::default();
        let mut languages = Languages::new();

        fs::create_dir_all(&child_dir)
            .unwrap_or_else(|_| panic!("Couldn't create {:?}", child_dir));
        fs::write(parent_dir.path().join(".ignore"), IGNORE_PATTERN)
            .expect("Couldn't create .gitignore.");
        fs::write(child_dir.join(FILE_NAME), FILE_CONTENTS).expect("Couldn't create child.rs");

        super::get_all_files(
            &[child_dir.as_path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.no_ignore_parent = Some(true);

        super::get_all_files(
            &[child_dir.as_path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn no_ignore_dot() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        fs::write(dir.path().join(".ignore"), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.no_ignore_dot = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn no_ignore_dot_still_vcs_gitignore() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        git2::Repository::init(dir.path()).expect("Couldn't create git repo.");

        fs::write(dir.path().join(".gitignore"), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        config.no_ignore_dot = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());
    }

    #[test]
    fn no_ignore_dot_includes_custom_ignore() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        fs::write(dir.path().join(IGNORE_FILE), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.no_ignore_dot = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn no_ignore_vcs_gitignore() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        git2::Repository::init(dir.path()).expect("Couldn't create git repo.");

        fs::write(dir.path().join(".gitignore"), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.no_ignore_vcs = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn no_ignore_vcs_gitignore_still_dot() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        fs::write(dir.path().join(".ignore"), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        config.no_ignore_vcs = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());
    }

    #[test]
    fn no_ignore_vcs_gitexclude() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let mut config = Config::default();
        let mut languages = Languages::new();

        git2::Repository::init(dir.path()).expect("Couldn't create git repo.");

        fs::write(dir.path().join(".git/info/exclude"), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        config.no_ignore_vcs = Some(true);

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn custom_ignore() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        git2::Repository::init(dir.path()).expect("Couldn't create git repo.");

        fs::write(dir.path().join(IGNORE_FILE), IGNORE_PATTERN).unwrap();
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_none());

        fs::remove_file(dir.path().join(IGNORE_FILE)).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &[],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn exclude_single_directory() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let vendor_dir = dir.path().join("vendor");
        fs::create_dir(&vendor_dir).expect("Couldn't create vendor dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(vendor_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["vendor"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 1);
        assert!(rust.reports[0].name.parent().unwrap() == dir.path());
    }

    #[test]
    fn exclude_multiple_directories() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let vendor_dir = dir.path().join("vendor");
        let node_modules_dir = dir.path().join("node_modules");
        fs::create_dir(&vendor_dir).expect("Couldn't create vendor dir");
        fs::create_dir(&node_modules_dir).expect("Couldn't create node_modules dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(vendor_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(node_modules_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["vendor", "node_modules"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 1);
        assert!(rust.reports[0].name.parent().unwrap() == dir.path());
    }

    #[test]
    fn exclude_nested_directory() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let src_dir = dir.path().join("src");
        let nested_vendor = src_dir.join("vendor");
        fs::create_dir_all(&nested_vendor).expect("Couldn't create nested dirs");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(src_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(nested_vendor.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["vendor"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 2);
        for report in &rust.reports {
            assert!(!report
                .name
                .components()
                .any(|c| c.as_os_str() == "vendor"));
        }
    }

    #[test]
    fn exclude_nonexistent_directory() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["nonexistent"],
            &mut languages,
            &config,
        );

        assert!(languages.get(LANGUAGE).is_some());
    }

    #[test]
    fn exclude_case_insensitive() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let vendor_dir = dir.path().join("Vendor");
        fs::create_dir(&vendor_dir).expect("Couldn't create Vendor dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(vendor_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["vendor"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 1);
    }

    #[test]
    fn exclude_glob_prefix() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let test_foo_dir = dir.path().join("test_foo");
        let test_bar_dir = dir.path().join("test_bar");
        let src_dir = dir.path().join("src");
        fs::create_dir(&test_foo_dir).expect("Couldn't create test_foo dir");
        fs::create_dir(&test_bar_dir).expect("Couldn't create test_bar dir");
        fs::create_dir(&src_dir).expect("Couldn't create src dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(test_foo_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(test_bar_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(src_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["test_*"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 2);
        for report in &rust.reports {
            assert!(!report
                .name
                .file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with("test_"));
        }
    }

    #[test]
    fn exclude_glob_suffix() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let foo_test_dir = dir.path().join("foo_test");
        let bar_test_dir = dir.path().join("bar_test");
        let src_dir = dir.path().join("src");
        fs::create_dir(&foo_test_dir).expect("Couldn't create foo_test dir");
        fs::create_dir(&bar_test_dir).expect("Couldn't create bar_test dir");
        fs::create_dir(&src_dir).expect("Couldn't create src dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(foo_test_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(bar_test_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(src_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["*_test"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 2);
        for report in &rust.reports {
            assert!(!report
                .name
                .file_name()
                .unwrap()
                .to_string_lossy()
                .ends_with("_test"));
        }
    }

    #[test]
    fn exclude_glob_single_char() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let test1_dir = dir.path().join("test1");
        let test2_dir = dir.path().join("test2");
        let test_dir = dir.path().join("test");
        fs::create_dir(&test1_dir).expect("Couldn't create test1 dir");
        fs::create_dir(&test2_dir).expect("Couldn't create test2 dir");
        fs::create_dir(&test_dir).expect("Couldn't create test dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(test1_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(test2_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(test_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["test?"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 2);
    }

    #[test]
    fn exclude_glob_path_pattern() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let src_vendor_dir = dir.path().join("src/vendor");
        let lib_vendor_dir = dir.path().join("lib/vendor");
        let src_dir = dir.path().join("src");
        let lib_dir = dir.path().join("lib");
        fs::create_dir_all(&src_vendor_dir).expect("Couldn't create src/vendor dir");
        fs::create_dir_all(&lib_vendor_dir).expect("Couldn't create lib/vendor dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(src_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(lib_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(src_vendor_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(lib_vendor_dir.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["*/vendor"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 3);
        for report in &rust.reports {
            let path_str = report.name.to_string_lossy();
            assert!(!path_str.contains("vendor"));
        }
    }

    #[test]
    fn exclude_glob_nested_all_levels() {
        let dir = TempDir::new().expect("Couldn't create temp dir.");
        let config = Config::default();
        let mut languages = Languages::new();

        let node_modules1 = dir.path().join("node_modules");
        let node_modules2 = dir.path().join("src/node_modules");
        let node_modules3 = dir.path().join("src/lib/node_modules");
        fs::create_dir_all(&node_modules1).expect("Couldn't create node_modules dir");
        fs::create_dir_all(&node_modules2).expect("Couldn't create src/node_modules dir");
        fs::create_dir_all(&node_modules3).expect("Couldn't create src/lib/node_modules dir");
        fs::write(dir.path().join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(dir.path().join("src").join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(node_modules1.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(node_modules2.join(FILE_NAME), FILE_CONTENTS).unwrap();
        fs::write(node_modules3.join(FILE_NAME), FILE_CONTENTS).unwrap();

        super::get_all_files(
            &[dir.path().to_str().unwrap()],
            &["node_modules"],
            &mut languages,
            &config,
        );

        let rust = languages.get(LANGUAGE).expect("Rust should exist");
        assert_eq!(rust.reports.len(), 2);
        for report in &rust.reports {
            assert!(!report
                .name
                .components()
                .any(|c| c.as_os_str() == "node_modules"));
        }
    }
}
