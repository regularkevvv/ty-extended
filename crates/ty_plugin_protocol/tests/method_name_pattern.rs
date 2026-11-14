//! Unit tests for the `*` glob used by `on-subclass-of-matching` method claims.

use ty_plugin_protocol::method_name_pattern_matches;

#[test]
fn star_alone_matches_everything() {
    assert!(method_name_pattern_matches("*", "run_a"));
    assert!(method_name_pattern_matches("*", "__init__"));
    assert!(method_name_pattern_matches("*", ""));
}

#[test]
fn no_star_is_exact_match() {
    assert!(method_name_pattern_matches("run_a", "run_a"));
    assert!(!method_name_pattern_matches("run_a", "run_b"));
    assert!(!method_name_pattern_matches("run", "run_a"));
}

#[test]
fn prefix_suffix_and_contains() {
    assert!(method_name_pattern_matches("run_*", "run_a"));
    assert!(method_name_pattern_matches("run_*", "run_"));
    assert!(!method_name_pattern_matches("run_*", "rerun_a"));
    assert!(method_name_pattern_matches("*_slow", "run_slow"));
    assert!(!method_name_pattern_matches("*_slow", "slow_run"));
    assert!(method_name_pattern_matches("*id*", "middle"));
    assert!(method_name_pattern_matches("*id*", "midday"));
    assert!(!method_name_pattern_matches("*id*", "mde"));
}

#[test]
fn interior_and_multiple_stars() {
    assert!(method_name_pattern_matches("a*c", "abc"));
    assert!(method_name_pattern_matches("a*c", "ac"));
    assert!(!method_name_pattern_matches("a*c", "acb"));
    assert!(method_name_pattern_matches("a*b*c", "aXXbYYc"));
    assert!(!method_name_pattern_matches("a*b*c", "acb"));
    assert!(method_name_pattern_matches("**", "anything"));
    assert!(method_name_pattern_matches("r**_a", "run_a"));
    assert!(method_name_pattern_matches("*run*", "rerun_x"));
}

#[test]
fn star_backtracks_across_partial_matches() {
    // Greedy `*` consumption must backtrack so the trailing literal can still anchor.
    assert!(method_name_pattern_matches("*a", "aa"));
    assert!(method_name_pattern_matches("a*a", "aa"));
    assert!(method_name_pattern_matches("*ab", "aab"));
    assert!(method_name_pattern_matches("a*b*b", "abab"));
    assert!(!method_name_pattern_matches("a*b*b", "abba"));
}

#[test]
fn empty_pattern_matches_nothing_real() {
    assert!(!method_name_pattern_matches("", "run_a"));
    assert!(method_name_pattern_matches("", ""));
}

#[test]
fn other_glob_metacharacters_are_literal() {
    assert!(!method_name_pattern_matches("run_?", "run_a"));
    assert!(method_name_pattern_matches("run_?", "run_?"));
    assert!(!method_name_pattern_matches("[gh]alt", "halt"));
}

#[test]
fn unicode_names_compare_byte_exact() {
    assert!(method_name_pattern_matches("*_méthode", "run_méthode"));
    assert!(!method_name_pattern_matches("*_method", "run_méthode"));
}
