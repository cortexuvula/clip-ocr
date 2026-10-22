#!/usr/bin/env python3
"""Curated tessdata guard proof: real assertion failure, restore, real pass.

Run from a disposable branch/worktree with no other source writer. No commits,
resets, history rewrites, installs or dependency additions. --linux is mandatory
for native cases and fails (never skips) on a non-Linux host. Logs live in target/.
"""
import argparse
from dataclasses import dataclass
import json
from pathlib import Path
import re
import subprocess
import sys

ROOT = Path(__file__).resolve().parents[1]
TESSDATA = "src-tauri/src/tessdata.rs"
ENGINE = "src-tauri/src/engine.rs"
LINUX_TESTS = "src-tauri/src/engine_linux_tests.rs"


@dataclass(frozen=True)
class Mutation:
    name: str
    path: str
    old: str
    new: str
    tests: tuple[str, ...]
    diagnostic: str = ""


def pure(name):
    return "tessdata::tests::" + name


def native(name):
    return "engine::linux_tests::" + name


RESOLVE = "dirs.iter().find(|d| has_language(d, lang)).cloned()"
CASES = [
    Mutation("missing-language-picks-existing-tree", TESSDATA, RESOLVE,
             "dirs.iter().find(|d| d.is_dir()).cloned()",
             (pure("resolves_the_directory_that_holds_the_language_not_merely_the_first"),)),
    Mutation("missing-language-invents-datapath", TESSDATA, RESOLVE,
             RESOLVE + ".or_else(|| dirs.first().cloned())",
             (pure("unresolvable_language_leaves_the_library_default_in_place"),)),
    Mutation("static-overrides-environment", TESSDATA,
             "if let Some(prefix) = env_prefix {\n        dirs.extend(prefix_candidates(prefix));\n    }\n    dirs.extend_from_slice(candidates);",
             "dirs.extend_from_slice(candidates);\n    if let Some(prefix) = env_prefix {\n        dirs.extend(prefix_candidates(prefix));\n    }",
             (pure("environment_prefix_may_be_the_tessdata_dir_or_its_parent"),)),
    Mutation("parent-prefix-not-supported", TESSDATA, "dirs.push(nested);", "let _ = nested;",
             (pure("environment_prefix_may_be_the_tessdata_dir_or_its_parent"),)),
    Mutation("nested-prefix-beats-direct-prefix", TESSDATA, "dirs.push(nested);", "dirs.insert(0, nested);",
             (pure("environment_prefix_may_be_the_tessdata_dir_or_its_parent"),)),
    Mutation("runtime-version-scan-disabled", TESSDATA, "std::fs::read_dir(versioned_parent)",
             'std::fs::read_dir(versioned_parent.join("missing-mutant-tree"))',
             (pure("runtime_versioned_scan_finds_languages_after_static_candidates"),)),
    Mutation("missing-scan-root-is-fatal", TESSDATA, "if let Ok(entries) = std::fs::read_dir(versioned_parent) {",
             'let _ = std::fs::read_dir(versioned_parent).expect("mutant: discovery root required");\n    if let Ok(entries) = std::fs::read_dir(versioned_parent) {',
             (pure("unusable_prefix_and_scan_paths_do_not_hide_a_valid_static_language"),)),
    Mutation("language-name-validation-removed", TESSDATA, "is_plain_language(lang) && ", "",
             (pure("language_names_cannot_escape_the_tessdata_directory"),)),
    Mutation("directories-masquerade-as-languages", TESSDATA,
             "(base != NON_LANGUAGE && has_language(dir, &base)).then_some(base)",
             "(base != NON_LANGUAGE && is_plain_language(&base)).then_some(base)",
             (pure("languages_ignore_traineddata_directories_and_continue_to_a_usable_tree"),)),
    Mutation("orientation-is-listed-as-language", TESSDATA,
             "(base != NON_LANGUAGE && has_language(dir, &base)).then_some(base)",
             "has_language(dir, &base).then_some(base)",
             (pure("languages_skip_orientation_data_and_report_the_first_populated_tree"),)),
    Mutation("later-tree-hides-first-usable-languages", TESSDATA, "return langs;", "return languages_in(&dirs[1..]);",
             (pure("languages_skip_orientation_data_and_report_the_first_populated_tree"),)),
]
NATIVE_CASES = [
    Mutation("null-datapath-regression", ENGINE,
             "tesseract::Tesseract::new(datapath.as_deref(), Some(&lang))",
             "tesseract::Tesseract::new(None, Some(&lang))",
             (native("an_unusable_tessdata_prefix_does_not_break_ocr"),
              native("relocated_language_uses_explicit_datapath_even_when_prefix_is_a_file"))),
    Mutation("unresolved-discovery-forbids-library-default", ENGINE,
             "let datapath = datapath_for(dirs, &lang);",
             'let datapath = datapath_for(dirs, &lang).or_else(|| Some("missing-tessdata-mutant".into()));',
             (native("unresolved_discovery_preserves_the_working_library_default"),)),
    Mutation("empty-languages-reported-as-supported", ENGINE,
             "if langs.is_empty() {\n            Err(Failure::Unsupported)",
             "if langs.is_empty() {\n            Ok(langs)",
             (native("languages_reject_empty_trees_and_accept_only_the_first_usable_partial_tree"),)),
    Mutation("normalization-bypassed", ENGINE, "let lang = normalize(language);", "let lang = language.to_owned();",
             (native("tesseract_reads_synthetic_text_and_reports_no_fabricated_confidence"),)),
    Mutation("normalize-skips-the-primary-subtag-mapping", ENGINE,
             '"de" | "deu" | "ger" => "deu",', '"de" | "deu" | "ger" => "de",',
             (native("normalize_maps_configured_tags_to_traineddata_codes"),)),
    Mutation("normalize-falls-back-to-english", ENGINE,
             "other => return other.to_owned(),", 'other => return "eng".to_owned(),',
             (native("normalize_never_falls_back_to_english_for_an_unknown_tag"),)),
    Mutation("missing-traineddata-prerequisite", LINUX_TESTS,
             'crate::tessdata::resolve(None, "eng").expect(PREREQUISITE)',
             'crate::tessdata::resolve_in(&[], "eng").expect(PREREQUISITE)',
             (native("tesseract_reads_synthetic_text_and_reports_no_fabricated_confidence"),),
             "install tesseract-ocr and tesseract-ocr-eng"),
    Mutation("child-filter-runs-zero-tests", LINUX_TESTS,
             '.args(["--exact", &full_name, "--nocapture"])',
             '.args(["--exact", "not_a_real_test", "--nocapture"])',
             (native("tesseract_reads_synthetic_text_and_reports_no_fabricated_confidence"),),
             "child filter must execute"),
    Mutation("negative-control-prefix-is-not-poisoned", LINUX_TESTS,
             '.env("TESSDATA_PREFIX", prefix);', '.env("TESSDATA_PREFIX", require_english_data());',
             (native("an_unusable_tessdata_prefix_does_not_break_ocr"),),
             "negative control: library default must fail"),
]


def run_test(test, expected, log):
    command = ["cargo", "test", "--locked", "--lib", test, "--", "--exact"]
    result = subprocess.run(command, cwd=ROOT, text=True, stdout=subprocess.PIPE,
                            stderr=subprocess.STDOUT, timeout=180)
    output = result.stdout
    log.write_text("$ " + " ".join(command) + "\n" + output)
    print(output, flush=True)
    assert result.returncode == expected, f"{test}: expected exit {expected}, got {result.returncode}"
    assert "running 1 test" in output, f"{test}: no test executed"
    if expected:
        assert "could not compile" not in output, "a compile failure is NOT mutation evidence"
        assert re.search(r"^    " + re.escape(test) + r"$", output, re.MULTILINE), "named assertion failure missing"
        assert "test result: FAILED" in output
    else:
        assert f"test {test} ... ok" in output, "named passing test missing"
    return output


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--linux", action="store_true", help="also run native Linux proof (requires installed English traineddata)")
    args = parser.parse_args()
    if args.linux and sys.platform != "linux":
        parser.error("--linux requires an actual Linux runner; macOS cannot compile/run cfg(target_os=linux)")
    cases = CASES + (NATIVE_CASES if args.linux else [])
    out = ROOT / "target" / "tessdata-mutations"
    out.mkdir(parents=True, exist_ok=True)
    results = []
    for case in cases:
        path = ROOT / case.path
        original = path.read_text()
        assert original.count(case.old) == 1, f"{case.name}: mutation anchor changed; review it rather than guessing"
        mutated = original.replace(case.old, case.new, 1)
        print(f"\n=== {case.name}: introduce defect ===", flush=True)
        path.write_text(mutated)
        try:
            for index, test in enumerate(case.tests):
                output = run_test(test, 101, out / f"{case.name}-{index}-red.log")
                assert not case.diagnostic or case.diagnostic in output, "required diagnostic missing"
        finally:
            # Do not overwrite another writer's tracked changes, even on failure.
            assert path.read_text() == mutated, f"{path}: unexpected external edit; STOP, refusing to restore"
            path.write_text(original)
        print(f"=== {case.name}: restore intended implementation ===", flush=True)
        for index, test in enumerate(case.tests):
            run_test(test, 0, out / f"{case.name}-{index}-green.log")
            results.append({"mutation": case.name, "test": test, "red_exit": 101, "green_exit": 0})
        assert path.read_text() == original, "source was not restored"
    summary = {"platform": sys.platform, "native_linux": args.linux, "proofs": results}
    (out / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    print(json.dumps(summary, indent=2), flush=True)


if __name__ == "__main__":
    main()
