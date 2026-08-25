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
    /// Compiles an ignore set from patterns.
    pub fn new(patterns: &[String]) -> Result<IgnoreSet> {
        let mut compiled = Vec::with_capacity(patterns.len());
        for pattern in patterns {
            compiled.push(Pattern::compile(pattern)?);
        }
        Ok(IgnoreSet { patterns: compiled })
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
