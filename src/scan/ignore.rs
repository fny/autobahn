//! Ignore pattern handling (gitignore-style globs).
//!
//! Patterns are compiled once, at construction, and then evaluated in order
//! against root-relative paths, with the last matching pattern deciding the
//! outcome. This ordering is what makes negations useful: `*.log` followed by
//! `!keep.log` ignores every log file except one, while the reverse order
//! ignores all of them.
//!
//! # Re-including beneath an ignored directory
//!
//! Scans do not descend into an ignored directory — that is the point of
//! ignoring one, and what keeps `node_modules` free. The exception is a
//! directory that a negation names something inside: `["node_modules",
//! "!node_modules/keep"]` walks `node_modules` after all, with everything
//! in it ignored except what a negation re-includes. So the two spellings
//! `.yarn` and `.yarn/*` agree once a negation is involved, where they
//! used not to. See [`IgnoreSet::holds_a_re_inclusion`].
//!
//! This is one place autobahn is deliberately more forgiving than git,
//! which refuses to re-include under an excluded directory. The change is
//! safe for every configuration that runs today: before it, such a
//! configuration was refused at startup, so nothing that synchronizes now
//! changes shape.

use anyhow::{bail, Context, Result};
use globset::{GlobBuilder, GlobMatcher};

/// A single compiled ignore pattern.
#[derive(Clone, Debug)]
struct Pattern {
    /// The compiled glob, expressed in root-relative terms (any-depth
    /// patterns carry an explicit `**/` prefix).
    matcher: GlobMatcher,
    /// Whether or not a match re-includes (rather than ignores) the path.
    negated: bool,
    /// Whether or not the pattern only applies to directories (a trailing
    /// `/` in the source pattern).
    directory_only: bool,
}

impl Pattern {
    /// Compiles a single source pattern.
    fn compile(pattern: &str) -> Result<Pattern> {
        let mut expression = pattern;

        // A leading '!' negates, and a trailing '/' restricts the pattern to
        // directories. Both are stripped before the glob is compiled.
        let negated = expression.starts_with('!');
        if negated {
            expression = &expression[1..];
        }
        let directory_only = expression.ends_with('/');
        if directory_only {
            expression = &expression[..expression.len() - 1];
        }
        if expression.is_empty() {
            bail!("invalid ignore pattern {pattern:?}: pattern has no content");
        }

        // A pattern without a separator matches at any depth, which is
        // expressed to globset as an explicit recursive prefix. A pattern
        // with a separator is anchored at the synchronization root, and
        // since scan paths carry no leading slash, neither may the glob.
        let expression = if expression.contains('/') {
            expression
                .strip_prefix('/')
                .unwrap_or(expression)
                .to_owned()
        } else {
            format!("**/{expression}")
        };
        if expression.is_empty() {
            bail!("invalid ignore pattern {pattern:?}: pattern has no content");
        }

        // Separators are literal so that '*' and '?' stay within a single
        // path component; '**' remains the only way to cross a boundary.
        let glob = GlobBuilder::new(&expression)
            .literal_separator(true)
            .build()
            .with_context(|| format!("invalid ignore pattern {pattern:?}"))?;
        Ok(Pattern {
            matcher: glob.compile_matcher(),
            negated,
            directory_only,
        })
    }
}

/// Every directory that a wildcard-free negation names something inside,
/// including the intermediate ones: `!a/b/c.txt` needs `a` and `a/b` both
/// walkable, or the walk stops before it ever reaches the file.
fn reachable_directories(patterns: &[Pattern]) -> std::collections::HashSet<String> {
    let mut reachable = std::collections::HashSet::new();
    for pattern in patterns.iter().filter(|pattern| pattern.negated) {
        let source = pattern.matcher.glob().glob();
        // Compiled any-depth patterns carry the prefix the compiler added;
        // the path they name is what follows it.
        let path = source.strip_prefix("**/").unwrap_or(source);
        // A wildcard anywhere makes the ancestors unknowable, and guessing
        // would mean walking directories on the chance that something
        // inside is re-included.
        if path.contains(['*', '?', '[']) {
            continue;
        }
        for (at, _) in path.match_indices('/') {
            reachable.insert(path[..at].to_owned());
        }
    }
    reachable
}

/// A compiled set of ignore patterns.
///
/// Patterns follow gitignore-style semantics over root-relative paths:
/// `name` matches at any depth, `/name` anchors to the root, trailing `/`
/// restricts to directories, `**` crosses directory boundaries, and a `!`
/// prefix negates (re-includes) previously ignored paths, with later
/// patterns taking precedence.
#[derive(Clone, Debug)]
pub struct IgnoreSet {
    /// The patterns, in source order.
    patterns: Vec<Pattern>,
    /// Directories that a negation names something inside, so that one
    /// of them being ignored does not prune the walk — its contents are
    /// then decided by the patterns, and the negation gets its say.
    ///
    /// Computed once, from the patterns alone. Only negations without
    /// wildcards contribute, because they are the only ones naming a
    /// directory this can know in advance; a negation that re-includes a
    /// directory *itself* (`!**/src/**/build/`) needs no help, since the
    /// walk never pruned there to begin with.
    reachable: std::collections::HashSet<String>,
}

impl Default for IgnoreSet {
    /// The empty ignore set, which ignores nothing.
    fn default() -> IgnoreSet {
        IgnoreSet::new(&[]).expect("the empty ignore set always compiles")
    }
}

impl IgnoreSet {
    /// A stable rendering of this set, for deciding whether two endpoints
    /// over one root would see the same tree — and so may share one
    /// observation of it. Two sets render identically exactly when they
    /// were compiled from the same patterns in the same order.
    pub fn key(&self) -> String {
        self.patterns
            .iter()
            .map(|pattern| {
                format!(
                    "{}{}{}",
                    if pattern.negated { "!" } else { "" },
                    pattern.matcher.glob().glob(),
                    if pattern.directory_only { "/" } else { "" },
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Compiles an ignore set from patterns.
    pub fn new(patterns: &[String]) -> Result<IgnoreSet> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            compiled.push(Pattern::compile(pattern)?);
        }
        let reachable = reachable_directories(&compiled);
        Ok(IgnoreSet {
            patterns: compiled,
            reachable,
        })
    }

    /// Whether the walk must enter this directory even though it is
    /// ignored, because a negation names something inside it.
    ///
    /// Asked only where the walk is about to prune — once per ignored
    /// directory, not once per entry — against a set built at compile
    /// time. A pattern list with no negations has an empty set, and the
    /// first test short-circuits before any hashing.
    pub fn holds_a_re_inclusion(&self, path: &str) -> bool {
        !self.reachable.is_empty() && self.reachable.contains(path)
    }

    /// Inside an ignored region, whether an explicit negation saves this
    /// entry. Everything else in the region is ignored by virtue of where
    /// it is, so the question is not "does anything ignore it" but "does
    /// anything re-include it". The last matching pattern decides, as ever.
    pub fn re_included(&self, path: &str, is_directory: bool) -> bool {
        for pattern in self.patterns.iter().rev() {
            if pattern.directory_only && !is_directory {
                continue;
            }
            if pattern.matcher.is_match(path) {
                return pattern.negated;
            }
        }
        false
    }

    /// Reports negations that can never re-include anything.
    ///
    /// Combining ignore files written independently is where these appear.
    /// Each file is written as though it were the only one, so a template
    /// that carefully says `!gradle-wrapper.jar` is undone by a later file
    /// saying `*.jar`, and the reader has no way to see it: the line is
    /// still there, and it does nothing.
    ///
    /// One cause, and always a mistake rather than a matter of taste: a
    /// later pattern ignores it again, so last-match-wins overwrites it
    /// with something further down the list.
    ///
    /// An ancestor being ignored used to belong here too. It no longer
    /// does — such a directory is walked anyway, so the negation is
    /// consulted and takes effect. See [`IgnoreSet::holds_a_re_inclusion`].
    ///
    /// Only negations with no wildcards are examined, because those are
    /// the only ones that name a path this can test directly. That is a
    /// deliberate floor: every report is a real dead line, and a pattern
    /// too general to decide is left alone rather than guessed at.
    pub fn dead_negations(&self) -> Vec<String> {
        let mut dead = Vec::new();
        for (index, pattern) in self.patterns.iter().enumerate() {
            if !pattern.negated {
                continue;
            }
            let source = pattern.matcher.glob().glob();
            // Compiled any-depth patterns carry the prefix the compiler
            // added; the path they name is what follows it.
            let path = source.strip_prefix("**/").unwrap_or(source);
            if path.contains(['*', '?', '[']) {
                continue;
            }

            if let Some(later) = self.patterns[index + 1..].iter().find(|later| {
                !later.negated && !later.directory_only && later.matcher.is_match(path)
            }) {
                dead.push(format!(
                    "!{path} can never take effect: {} later ignores it again",
                    later.matcher.glob().glob()
                ));
                continue;
            }
        }
        dead
    }

    /// Reports wildcard negations that an ignored directory above them
    /// makes do nothing.
    ///
    /// `vendor` with `!vendor/*.patch` re-includes nothing: a wildcard
    /// negation cannot say in advance which directories hold what it
    /// matches, so it opens none (see [`IgnoreSet::holds_a_re_inclusion`]),
    /// and the walk prunes at `vendor` before the negation is ever asked.
    /// Only the negation's literal leading directories are examined — a
    /// pattern like `!**/*.patch` names no directory to test — and each is
    /// decided exactly as the walk decides it, so a directory that another,
    /// wildcard-free negation opens is not reported.
    ///
    /// These are warnings, not errors: a configuration that relies on them
    /// ran before, and still does, only without the effect it meant.
    pub fn ineffective_negations(&self) -> Vec<String> {
        let mut ineffective = Vec::new();
        for pattern in self.patterns.iter().filter(|pattern| pattern.negated) {
            let path = pattern.matcher.glob().glob();
            if !path.contains(['*', '?', '[']) {
                continue;
            }
            if let Some(directory) = self.pruned_ancestor(path) {
                ineffective.push(format!(
                    "!{path} has no effect: {directory} is ignored, and a negation with a \
                     wildcard does not open an ignored directory. Ignore its contents \
                     instead ({directory}/*), or name what to keep without a wildcard"
                ));
            }
        }
        ineffective
    }

    /// The first directory above `path` at which a walk from the root
    /// stops: one that is ignored and that no negation opens. Mirrors the
    /// scanner, which inverts the question inside an ignored region it
    /// was let into. Directories spelled with a wildcard end the search,
    /// since they name no one directory.
    fn pruned_ancestor<'a>(&self, path: &'a str) -> Option<&'a str> {
        let mut within_ignored = false;
        for (at, _) in path.match_indices('/') {
            let directory = &path[..at];
            if directory.contains(['*', '?', '[']) {
                break;
            }
            let ignored = match within_ignored {
                true => !self.re_included(directory, true),
                false => self.ignored(directory, true),
            };
            if ignored && !self.holds_a_re_inclusion(directory) {
                return Some(directory);
            }
            within_ignored = ignored;
        }
        None
    }

    /// Indicates whether or not the root-relative path (with `is_directory`
    /// disambiguating directory-only patterns) is ignored.
    pub fn ignored(&self, path: &str, is_directory: bool) -> bool {
        // The last matching pattern decides the outcome, so the search runs
        // backwards and stops at the first match it finds — equivalent to
        // scanning forwards while overwriting the result, but it does less
        // matching work.
        for pattern in self.patterns.iter().rev() {
            if pattern.directory_only && !is_directory {
                continue;
            }
            if pattern.matcher.is_match(path) {
                return !pattern.negated;
            }
        }
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ignores(patterns: &[&str]) -> IgnoreSet {
        let patterns: Vec<String> = patterns.iter().map(|p| (*p).to_owned()).collect();
        IgnoreSet::new(&patterns).expect("patterns should compile")
    }

    /// The exact clash that combining independent templates produces:
    /// one file protects a file by name, a later file ignores it by kind.
    #[test]
    fn a_negation_undone_by_a_later_pattern_is_reported() {
        let set = IgnoreSet::new(&[
            "*.jar".to_owned(),
            "!gradle-wrapper.jar".to_owned(),
            "*.jar".to_owned(),
        ])
        .expect("compiles");
        let dead = set.dead_negations();
        assert_eq!(dead.len(), 1, "{dead:?}");
        assert!(dead[0].contains("gradle-wrapper.jar"), "{dead:?}");
        assert!(dead[0].contains("later ignores it again"), "{dead:?}");
    }

    /// A negation that wins is not reported: it is the whole reason the
    /// last-match-wins ordering exists.
    #[test]
    fn a_negation_that_takes_effect_is_left_alone() {
        let set = IgnoreSet::new(&["*.jar".to_owned(), "!gradle-wrapper.jar".to_owned()])
            .expect("compiles");
        assert!(set.dead_negations().is_empty());
        assert!(!set.ignored("gradle-wrapper.jar", false));
    }

    /// Re-including beneath an ignored directory used to be impossible:
    /// the walk pruned at the directory, so nothing below was ever tested.
    /// The set now tells the walk to enter it, and the negation takes
    /// effect — so the two spellings agree, and this is no longer dead.
    #[test]
    fn a_negation_beneath_an_ignored_directory_is_live() {
        let set = ignores(&[".yarn", "!.yarn/patches"]);
        assert!(
            set.dead_negations().is_empty(),
            "{:?}",
            set.dead_negations()
        );
        assert!(
            set.holds_a_re_inclusion(".yarn"),
            "the walk must enter .yarn to reach the re-inclusion"
        );
        assert!(set.re_included(".yarn/patches", true));
        assert!(!set.re_included(".yarn/cache", true), "still ignored");
    }

    /// Every level has to stay walkable, or the walk stops before it
    /// reaches the file.
    #[test]
    fn a_deep_re_inclusion_keeps_every_level_walkable() {
        let set = ignores(&["a", "!a/b/c.txt"]);
        assert!(set.holds_a_re_inclusion("a"));
        assert!(set.holds_a_re_inclusion("a/b"));
        assert!(
            !set.holds_a_re_inclusion("a/b/c.txt"),
            "the file is not a door"
        );
    }

    /// A pattern list with no negations pays nothing: the set is empty
    /// and the walk prunes exactly as before.
    #[test]
    fn without_negations_nothing_is_kept_walkable() {
        let set = ignores(&["node_modules", "target"]);
        assert!(!set.holds_a_re_inclusion("node_modules"));
        assert!(!set.holds_a_re_inclusion("target"));
    }

    /// A negation that re-includes a directory *itself* needs no help —
    /// the walk never pruned there — and a wildcard one contributes no
    /// door, because its ancestors cannot be known in advance.
    #[test]
    fn a_wildcard_negation_contributes_no_door() {
        let set = ignores(&["build/", "!**/src/**/build/"]);
        assert!(!set.holds_a_re_inclusion("src"));
        assert!(
            !set.ignored("src/x/build", true),
            "re-included on its own merits"
        );
    }

    /// The correct idiom for the case above — ignore the contents, not the
    /// directory — must not be reported. This is what the real templates
    /// use, so a false positive here would make the check unusable.
    #[test]
    fn ignoring_the_contents_rather_than_the_directory_is_correct() {
        let set =
            IgnoreSet::new(&[".yarn/*".to_owned(), "!.yarn/patches".to_owned()]).expect("compiles");
        assert!(
            set.dead_negations().is_empty(),
            "{:?}",
            set.dead_negations()
        );
    }

    /// A negation general enough that no single path decides it is left
    /// alone: the check reports only what it can prove.
    #[test]
    fn a_wildcard_negation_is_not_guessed_at() {
        let set = IgnoreSet::new(&["build/".to_owned(), "!**/src/**/build/".to_owned()])
            .expect("compiles");
        assert!(set.dead_negations().is_empty());
    }

    #[test]
    fn empty_set_ignores_nothing() {
        let set = ignores(&[]);
        assert!(!set.ignored("anything", false));
        assert!(!set.ignored("any/thing", true));
    }

    #[test]
    fn separator_free_patterns_match_at_any_depth() {
        let set = ignores(&["build"]);
        assert!(set.ignored("build", true));
        assert!(set.ignored("a/build", true));
        assert!(set.ignored("a/b/build", false));
        assert!(!set.ignored("rebuild", false));
        assert!(!set.ignored("build/inner", false));
    }

    #[test]
    fn patterns_with_separators_are_root_anchored() {
        let set = ignores(&["/target", "a/b"]);
        assert!(set.ignored("target", true));
        assert!(!set.ignored("nested/target", true));
        assert!(set.ignored("a/b", false));
        assert!(!set.ignored("x/a/b", false));
    }

    #[test]
    fn wildcards_do_not_cross_separators_but_double_star_does() {
        let set = ignores(&["*.log", "/tmp/*", "a/**/c"]);
        assert!(set.ignored("server.log", false));
        assert!(set.ignored("deep/nested/server.log", false));
        assert!(set.ignored("tmp/one", false));
        assert!(!set.ignored("tmp/one/two", false));
        assert!(set.ignored("a/b/c", false));
        assert!(set.ignored("a/b/x/c", false));
    }

    #[test]
    fn trailing_slash_restricts_to_directories() {
        let set = ignores(&["cache/"]);
        assert!(set.ignored("cache", true));
        assert!(!set.ignored("cache", false));
        assert!(set.ignored("a/cache", true));
    }

    #[test]
    fn last_matching_pattern_wins() {
        let set = ignores(&["*.log", "!keep.log"]);
        assert!(set.ignored("noise.log", false));
        assert!(!set.ignored("keep.log", false));
        assert!(!set.ignored("nested/keep.log", false));

        // The same patterns in the opposite order ignore everything, since
        // the broad pattern now has the final say.
        let reversed = ignores(&["!keep.log", "*.log"]);
        assert!(reversed.ignored("keep.log", false));
        assert!(reversed.ignored("noise.log", false));
    }

    #[test]
    fn negation_of_a_directory_only_pattern_respects_the_type() {
        let set = ignores(&["state", "!state/"]);
        // The negation only applies to directories, so a file named "state"
        // remains ignored while a directory is re-included.
        assert!(set.ignored("state", false));
        assert!(!set.ignored("state", true));
    }

    #[test]
    fn negation_beneath_an_ignored_directory_is_reported_but_unreachable() {
        // The ignore set answers for the path itself...
        let set = ignores(&["node_modules", "!node_modules/keep"]);
        assert!(set.ignored("node_modules", true));
        assert!(!set.ignored("node_modules/keep", false));
        // ...but scans never descend into "node_modules", so this negation
        // has no effect in practice. See the module documentation. The
        // supported spelling ignores the directory's contents instead.
        let supported = ignores(&["node_modules/*", "!node_modules/keep"]);
        assert!(!supported.ignored("node_modules", true));
        assert!(supported.ignored("node_modules/other", false));
        assert!(!supported.ignored("node_modules/keep", false));
    }

    /// The scanner's verdict on one path: whether the walk reaches it and,
    /// if so, whether it is carried. Built from the same questions the
    /// scanner asks, in the same order.
    fn carried(set: &IgnoreSet, path: &str, is_directory: bool) -> bool {
        let mut within_ignored = false;
        for (at, _) in path.match_indices('/') {
            let directory = &path[..at];
            let ignored = match within_ignored {
                true => !set.re_included(directory, true),
                false => set.ignored(directory, true),
            };
            if ignored && !set.holds_a_re_inclusion(directory) {
                return false;
            }
            within_ignored = ignored;
        }
        match within_ignored {
            true => set.re_included(path, is_directory),
            false => !set.ignored(path, is_directory),
        }
    }

    /// `vendor` with `!vendor/*.patch` re-includes nothing, and says so,
    /// pointing at the spelling that works.
    #[test]
    fn a_wildcard_negation_under_an_ignored_directory_is_reported() {
        let set = ignores(&["vendor", "!vendor/*.patch"]);
        let ineffective = set.ineffective_negations();
        assert_eq!(ineffective.len(), 1, "{ineffective:?}");
        assert!(
            ineffective[0].contains("!vendor/*.patch"),
            "{ineffective:?}"
        );
        assert!(ineffective[0].contains("has no effect"), "{ineffective:?}");
        assert!(ineffective[0].contains("vendor/*"), "{ineffective:?}");
        assert!(!carried(&set, "vendor/fix.patch", false));

        // The suggested spelling works, and is not reported.
        let fixed = ignores(&["vendor/*", "!vendor/*.patch"]);
        assert!(fixed.ineffective_negations().is_empty());
        assert!(carried(&fixed, "vendor/fix.patch", false));
    }

    /// Not reported where the negation does take effect: a directory a
    /// wildcard-free negation opens is walked, and a negation naming no
    /// directory is not guessed at.
    #[test]
    fn a_wildcard_negation_that_can_take_effect_is_not_reported() {
        let opened = ignores(&["vendor", "!vendor/keep.txt", "!vendor/*.patch"]);
        assert!(opened.ineffective_negations().is_empty());
        assert!(carried(&opened, "vendor/fix.patch", false));
        assert!(ignores(&["vendor", "!**/*.patch"])
            .ineffective_negations()
            .is_empty());
        assert!(ignores(&["build/", "!**/src/**/build/"])
            .ineffective_negations()
            .is_empty());
        assert!(ignores(&["*.log", "!keep*.log"])
            .ineffective_negations()
            .is_empty());
    }

    /// Inside a walked region, a negation applies wherever it matches, so
    /// `!*.md` re-includes `vendor/README.md` — but not a file in a
    /// directory the region still prunes. That is git's result for the
    /// `vendor/*` spelling (checked against `git status` on this tree),
    /// which is the spelling this one is meant to agree with.
    #[test]
    fn a_negation_inside_a_walked_region_matches_git() {
        let region = ignores(&["vendor", "!vendor/keep.txt", "!*.md"]);
        let git = ignores(&["vendor/*", "!vendor/keep.txt", "!*.md"]);
        // What `git status --untracked-files=all` lists for the second list.
        let expected = [
            ("top.md", false, true),
            ("vendor/README.md", false, true),
            ("vendor/keep.txt", false, true),
            ("vendor/junk.txt", false, false),
            ("vendor/sub/x.md", false, false),
        ];
        for (path, is_directory, listed) in expected {
            assert_eq!(carried(&region, path, is_directory), listed, "{path}");
            assert_eq!(carried(&git, path, is_directory), listed, "{path}");
        }
    }

    #[test]
    fn invalid_patterns_are_rejected_by_name() {
        let error = IgnoreSet::new(&["a[".to_owned()]).expect_err("unclosed class is invalid");
        assert!(format!("{error:#}").contains("a["));
        assert!(IgnoreSet::new(&[String::new()]).is_err());
        assert!(IgnoreSet::new(&["!".to_owned()]).is_err());
        assert!(IgnoreSet::new(&["/".to_owned()]).is_err());
    }
}
