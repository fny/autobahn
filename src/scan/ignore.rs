//! Ignore pattern handling (gitignore-style globs).
//!
//! Patterns are compiled once, at construction, and then evaluated in order
//! against root-relative paths, with the last matching pattern deciding the
//! outcome. This ordering is what makes negations useful: `*.log` followed by
//! `!keep.log` ignores every log file except one, while the reverse order
//! ignores all of them.
//!
//! # Limitation
//!
//! A negation cannot resurrect content beneath an ignored directory. Scans
//! never descend into an ignored directory (that is the entire point of
//! ignoring one), so no path inside it is ever tested against the pattern
//! list. The pattern list `["node_modules", "!node_modules/keep"]` therefore
//! leaves `node_modules/keep` unscanned, even though [`IgnoreSet::ignored`]
//! reports that path itself as unignored when asked directly. Re-including
//! content requires that its ancestors remain unignored, e.g. by ignoring
//! `node_modules/*` and then re-including `node_modules/keep`.

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
        Ok(IgnoreSet { patterns: compiled })
    }

    /// Reports negations that can never re-include anything.
    ///
    /// Combining ignore files written independently is where these appear.
    /// Each file is written as though it were the only one, so a template
    /// that carefully says `!gradle-wrapper.jar` is undone by a later file
    /// saying `*.jar`, and the reader has no way to see it: the line is
    /// still there, and it does nothing.
    ///
    /// Two causes, both always a mistake rather than a matter of taste:
    ///
    /// - A later pattern ignores it again. Last match wins, so the
    ///   negation is overwritten by something further down the list.
    /// - An ancestor directory is ignored. Scans never descend into an
    ///   ignored directory, so nothing inside is ever tested — the
    ///   limitation described at the top of this module.
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

            // Every ancestor, nearest first: the innermost ignored one is
            // the directory the scan actually stops at.
            let mut ancestors: Vec<&str> =
                path.match_indices('/').map(|(at, _)| &path[..at]).collect();
            ancestors.reverse();
            if let Some(ancestor) = ancestors
                .into_iter()
                .find(|ancestor| self.ignored(ancestor, true))
            {
                dead.push(format!(
                    "!{path} can never take effect: its directory {ancestor} is ignored, and \
                     scans do not descend into an ignored directory"
                ));
            }
        }
        dead
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

    /// Scans never descend into an ignored directory, so re-including
    /// something inside one cannot work however it is written.
    #[test]
    fn a_negation_beneath_an_ignored_directory_is_reported() {
        let set =
            IgnoreSet::new(&[".yarn".to_owned(), "!.yarn/patches".to_owned()]).expect("compiles");
        let dead = set.dead_negations();
        assert_eq!(dead.len(), 1, "{dead:?}");
        assert!(dead[0].contains(".yarn/patches"), "{dead:?}");
        assert!(dead[0].contains("is ignored"), "{dead:?}");
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

    #[test]
    fn invalid_patterns_are_rejected_by_name() {
        let error = IgnoreSet::new(&["a[".to_owned()]).expect_err("unclosed class is invalid");
        assert!(format!("{error:#}").contains("a["));
        assert!(IgnoreSet::new(&[String::new()]).is_err());
        assert!(IgnoreSet::new(&["!".to_owned()]).is_err());
        assert!(IgnoreSet::new(&["/".to_owned()]).is_err());
    }
}
