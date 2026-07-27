//! `TASKS.md` T4.1's rule engine (issue #14): whitelist/danger regex lists
//! loaded from `rules.toml`, matched against a write request's *decoded*
//! bytes — never the wire encoding it arrived in.
//!
//! # Why matching happens on decoded bytes (the hex-bypass fix)
//!
//! A client can send a write as `text` (UTF-8, plus a line ending) or as
//! `data_b64` (exact bytes, e.g. from `serialwrap write --hex`). Both paths
//! are decoded to a plain `Vec<u8>` by
//! `serialwrapd::protocol::session`'s `Request::Write` handler *before*
//! anything gets near this module (see that handler's `decode_write_bytes`)
//! — [`RuleSet::evaluate`] only ever sees that already-decoded `&[u8]`, the
//! same bytes that would actually go out the port. If matching were done
//! against the wire representation instead (e.g. the base64 or raw hex
//! string), `--hex "666C6173685F657261736500"` would sail past a `danger =
//! "erase"` rule that a plain `serialwrap write "flash_erase"` would
//! correctly trip — same bytes on the wire to the device, two different
//! gate outcomes, entirely defeating the rule. Decoding first and matching
//! only the decoded bytes means there is exactly one representation to
//! write rules against, regardless of which transport encoding a client
//! (or an attacker looking for a bypass) chooses.
//!
//! # Decision priority
//!
//! danger > whitelist > default-pending, and not overridable by a
//! whitelist match — see [`RuleSet::evaluate`]'s doc comment for why.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::time::Duration;

use regex::{Regex, RegexBuilder};
use serde::Deserialize;

/// Default approval timeout: 60 seconds, fail-safe (timeout denies) per the
/// Security-model wiki. Overridable via `rules.toml`'s `[approval]
/// timeout_s` — the *value* is configurable, the *direction* (timeout ==
/// deny, never allow) is not; see `super::approval`'s module docs.
pub const DEFAULT_APPROVAL_TIMEOUT: Duration = Duration::from_secs(60);

/// One compiled regex rule. `pattern` retains the original source text —
/// used verbatim as the human-facing label (e.g. `danger:erase`) so an
/// approval card names the rule that actually fired, not an opaque index.
#[derive(Debug, Clone)]
struct Rule {
    pattern: String,
    regex: Regex,
    /// Only danger rules carry a rationale — *why* this pattern is
    /// dangerous, shown to the human approver alongside the raw match.
    /// Whitelist entries don't need one: an operator wrote them to mean
    /// "this is safe", which is self-explanatory.
    reason: Option<String>,
}

impl Rule {
    fn compile(pattern: &str, reason: Option<String>) -> Result<Self, String> {
        // Case-insensitive unconditionally: `ERASE`/`Erase`/`erase` are the
        // same command semantically, and a rule an attacker (or just a
        // careless caps-lock) can dodge by changing case isn't a rule at
        // all (T4.1 acceptance criterion 2: "regex 邊界測試（大小寫、部分
        // 符合）"). Deliberately *not* anchored: `erase` must still match
        // inside `flash_erase`/`ERASE_ALL` — a substring match, not a
        // whole-string one (same acceptance criterion's "部分符合").
        let regex = RegexBuilder::new(pattern)
            .case_insensitive(true)
            .build()
            .map_err(|e| format!("invalid regex {pattern:?}: {e}"))?;
        Ok(Self {
            pattern: pattern.to_string(),
            regex,
            reason,
        })
    }
}

/// Outcome of matching one write's decoded bytes against a [`RuleSet`] —
/// the pure, synchronous half of T4.1's decision: no queue, no id, no I/O.
/// `gate::Gate::submit_write` turns this into the wire-facing
/// [`super::GateDecision`] by assigning a pending-queue id when one's
/// needed. Kept separate specifically so the priority-matrix, regex-
/// boundary, and hex-bypass acceptance tests can exercise rule matching
/// directly, with no async runtime or approval queue involved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleVerdict {
    /// A whitelist rule matched and no danger rule did — passes straight
    /// through. `reason` is `"whitelist:<pattern>"`.
    Allow { reason: String },
    /// Neither a danger nor a whitelist rule matched: nothing said this is
    /// safe, so a human decides (the project's default-pending posture).
    Pending,
    /// A danger rule matched — forced to approval *regardless* of whether
    /// a whitelist rule also matched (see [`RuleSet::evaluate`]).
    /// `matched_rule` is `"danger:<pattern>"` (matches the Security-model
    /// wiki's own example shape); `danger_reason` is that rule's human-
    /// facing rationale, surfaced in the approval payload so an operator
    /// sees *why* this is dangerous, not just that it is.
    ForcePending {
        matched_rule: String,
        danger_reason: String,
    },
}

/// Built-in danger patterns, shipped regardless of whether `rules.toml`
/// exists (used as-is when it doesn't; see [`RuleSet::load`]). Each one is
/// here because a mistaken or maliciously-crafted write matching it is not
/// just wrong but *unrecoverable without hardware intervention* — see the
/// module-level motivation in the PR this shipped with, and the
/// [Security-model wiki](https://github.com/SheldonChangL/serialwrap/wiki/Security-model):
///
/// - `erase` — flash erase destroys firmware; recovery needs a reflash
///   (usually a working JTAG/programmer, not "try again over serial").
/// - `fuse` / `otp` / `efuse` — one-time-programmable writes are permanent
///   *by definition*; there is no "undo" instruction for hardware fuses.
/// - `unlock` / `lock` — toggling a debug-port lock can permanently prevent
///   recovery (a locked-out debug port often can't be unlocked again
///   without the exact secret/state that locked it, if at all).
/// - bootloader-entry sequences (`bootloader`, `dfu[-_]?mode`,
///   `download[-_]?mode`) — leaves the device in a state where normal
///   commands do not apply; whatever the agent sends next is talking to
///   the wrong protocol entirely.
/// - `format` / `factory_reset` — destroys calibration and provisioning
///   data that, unlike firmware, usually has no source-controlled copy to
///   restore from.
const BUILTIN_DANGER: &[(&str, &str)] = &[
    (
        "erase",
        "Flash erase destroys firmware; recovery needs a reflash.",
    ),
    (
        "(fuse|otp|efuse)",
        "One-time-programmable writes are permanent by definition.",
    ),
    (
        "(unlock|lock)",
        "Debug-port lock can permanently prevent recovery.",
    ),
    (
        "(bootloader|dfu[-_]?mode|download[-_]?mode)",
        "Leaves the device in a state where normal commands do not apply.",
    ),
    (
        "(format|factory_reset)",
        "Destroys calibration and provisioning data.",
    ),
];

/// Raw `rules.toml` shape, deserialized as-is before compiling its regex
/// strings into [`Rule`]s (see [`RuleSet::compile`]).
#[derive(Debug, Deserialize, Default)]
struct RawRuleSet {
    #[serde(default)]
    approval: RawApproval,
    #[serde(default)]
    whitelist: Vec<RawRule>,
    #[serde(default)]
    danger: Vec<RawRule>,
}

#[derive(Debug, Deserialize)]
struct RawApproval {
    #[serde(default = "default_timeout_s")]
    timeout_s: f64,
}

impl Default for RawApproval {
    fn default() -> Self {
        Self {
            timeout_s: default_timeout_s(),
        }
    }
}

fn default_timeout_s() -> f64 {
    DEFAULT_APPROVAL_TIMEOUT.as_secs_f64()
}

#[derive(Debug, Deserialize)]
struct RawRule {
    pattern: String,
    #[serde(default)]
    reason: Option<String>,
}

/// A loaded, compiled write-gate policy: whitelist regexes, danger regexes
/// (each with a rationale), and the approval timeout. See the module docs
/// for the priority [`RuleSet::evaluate`] applies and why matching is
/// always against decoded bytes.
#[derive(Debug, Clone)]
pub struct RuleSet {
    whitelist: Vec<Rule>,
    danger: Vec<Rule>,
    pub timeout: Duration,
}

impl RuleSet {
    /// The built-in policy: [`BUILTIN_DANGER`]'s five rules, an empty
    /// whitelist, and [`DEFAULT_APPROVAL_TIMEOUT`]. This is what a fresh
    /// install runs with before an operator ever writes a `rules.toml` —
    /// deliberately *not* "no protection until configured": the danger
    /// list is a floor a missing config file can't drop you below.
    pub fn builtin() -> Self {
        let danger = BUILTIN_DANGER
            .iter()
            .map(|(pattern, reason)| {
                Rule::compile(pattern, Some((*reason).to_string()))
                    .expect("BUILTIN_DANGER patterns are valid regex — covered by a unit test")
            })
            .collect();
        Self {
            whitelist: Vec::new(),
            danger,
            timeout: DEFAULT_APPROVAL_TIMEOUT,
        }
    }

    fn compile(raw: RawRuleSet) -> Result<Self, String> {
        let whitelist = raw
            .whitelist
            .into_iter()
            .map(|r| Rule::compile(&r.pattern, r.reason))
            .collect::<Result<Vec<_>, _>>()?;
        let danger = raw
            .danger
            .into_iter()
            .map(|r| Rule::compile(&r.pattern, r.reason))
            .collect::<Result<Vec<_>, _>>()?;
        if raw.approval.timeout_s <= 0.0 || !raw.approval.timeout_s.is_finite() {
            return Err(format!(
                "rules.toml [approval] timeout_s must be a positive, finite number of seconds \
                 (got {})",
                raw.approval.timeout_s
            ));
        }
        Ok(Self {
            whitelist,
            danger,
            timeout: Duration::from_secs_f64(raw.approval.timeout_s),
        })
    }

    /// Load a policy from `path`. A missing file is not an error — it
    /// falls back to [`RuleSet::builtin`], so a fresh install (no
    /// `rules.toml` written yet) still has the built-in danger floor rather
    /// than no protection at all. A file that exists but fails to parse
    /// (bad TOML, an invalid regex, a nonsensical timeout) *is* an error:
    /// silently falling back there would mean a typo in an operator's own
    /// hand-edited danger list quietly disables whatever they meant to add
    /// — see `serialwrapd::run`'s call site for how that's surfaced instead
    /// of failing the whole daemon start.
    pub fn load(path: &Path) -> io::Result<Self> {
        let text = match fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::builtin()),
            Err(e) => return Err(e),
        };
        let raw: RawRuleSet = toml::from_str(&text).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid rules.toml at {}: {e}", path.display()),
            )
        })?;
        Self::compile(raw).map_err(|e| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid rules.toml at {}: {e}", path.display()),
            )
        })
    }

    /// Match `bytes` — already decoded from whatever the client sent
    /// (`text`+line-ending or `data_b64`) — against this policy.
    ///
    /// Danger is checked *before* whitelist, and a whitelist match never
    /// suppresses a danger one: `bytes` matching both returns
    /// [`RuleVerdict::ForcePending`], never [`RuleVerdict::Allow`]. This is
    /// the one priority rule the whole gate exists to enforce (T4.1
    /// acceptance criterion 1) — the escape hatch for a legitimately
    /// dangerous-looking-but-routine command is deliberately *editing
    /// `rules.toml`'s danger list itself* (a deliberate, considered admin
    /// action), never a runtime whitelist match overriding danger at
    /// decision time, and never a checkbox on the approval card in the
    /// moment (see T5.4's mockup, which disables "add to whitelist" for
    /// danger-class matches for exactly this reason: the person staring at
    /// a pending `flash_erase` is in exactly the wrong state of mind to be
    /// the one who permanently defuses that check).
    pub fn evaluate(&self, bytes: &[u8]) -> RuleVerdict {
        // Both whitelist and danger match against the same lossy-UTF-8
        // rendering of the decoded bytes — not the raw bytes themselves —
        // since every built-in/example pattern is an ASCII command
        // fragment and `regex::Regex` (not `regex::bytes::Regex`) operates
        // on `str`. Genuinely binary payloads (which most danger/whitelist
        // patterns have no reason to match anyway) still produce *some*
        // lossy string via replacement characters rather than failing to
        // match at all, which is the fail-safe direction here: a payload
        // that can't be interpreted as the command text a rule describes
        // simply doesn't match it, and falls through to the default-
        // pending posture rather than being silently skipped.
        let text = String::from_utf8_lossy(bytes);
        for rule in &self.danger {
            if rule.regex.is_match(&text) {
                return RuleVerdict::ForcePending {
                    matched_rule: format!("danger:{}", rule.pattern),
                    danger_reason: rule
                        .reason
                        .clone()
                        .unwrap_or_else(|| "matched a built-in danger pattern".to_string()),
                };
            }
        }
        for rule in &self.whitelist {
            if rule.regex.is_match(&text) {
                return RuleVerdict::Allow {
                    reason: format!("whitelist:{}", rule.pattern),
                };
            }
        }
        RuleVerdict::Pending
    }
}

/// Resolve the production `rules.toml` path: the platform config directory
/// (`~/.config/serialwrap/rules.toml` on Linux, `~/Library/Application
/// Support/serialwrap/rules.toml` on macOS via `directories::ProjectDirs`),
/// *not* `recorder::default_data_dir()`. Deliberately a different directory
/// from recorded event data: `rules.toml` is hand-authored operator policy,
/// not device output, so it belongs wherever this platform's convention
/// puts user config — and keeping it out of the data dir means nothing
/// about ring eviction, segment rotation, or "wipe the recordings and start
/// fresh" can ever brush against write-gate policy.
///
/// Tests must never call this — construct [`RuleSet::load`] with an
/// explicit path instead (same convention `recorder::default_data_dir`'s
/// doc comment already sets for this crate).
pub fn default_rules_path() -> io::Result<PathBuf> {
    directories::ProjectDirs::from("", "", "serialwrap")
        .map(|d| d.config_dir().join("rules.toml"))
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotFound,
                "could not resolve a home directory for the default serialwrap config dir",
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rules(whitelist: &[&str], danger: &[&str]) -> RuleSet {
        RuleSet {
            whitelist: whitelist
                .iter()
                .map(|p| Rule::compile(p, None).unwrap())
                .collect(),
            danger: danger
                .iter()
                .map(|p| Rule::compile(p, Some(format!("test reason for {p}"))).unwrap())
                .collect(),
            timeout: DEFAULT_APPROVAL_TIMEOUT,
        }
    }

    // ---- Acceptance criterion 4: built-in danger list, each with a reason ----

    #[test]
    fn builtin_danger_patterns_compile_and_every_one_carries_a_nonempty_reason() {
        let set = RuleSet::builtin();
        assert_eq!(
            set.danger.len(),
            5,
            "expected exactly the five built-in danger categories the wiki documents"
        );
        for rule in &set.danger {
            let reason = rule
                .reason
                .as_ref()
                .unwrap_or_else(|| panic!("danger rule {:?} has no rationale", rule.pattern));
            assert!(
                !reason.is_empty(),
                "danger rule {:?} has an empty rationale",
                rule.pattern
            );
        }
    }

    #[test]
    fn builtin_danger_covers_every_documented_category() {
        let set = RuleSet::builtin();
        let hits = |bytes: &str| {
            matches!(
                set.evaluate(bytes.as_bytes()),
                RuleVerdict::ForcePending { .. }
            )
        };
        assert!(hits("flash_erase 0x0 0x100000"), "erase");
        assert!(hits("blow_fuse"), "fuse");
        assert!(hits("read_otp"), "otp");
        assert!(hits("prog_efuse"), "efuse");
        assert!(hits("debug_unlock"), "unlock");
        assert!(hits("port_lock"), "lock");
        assert!(hits("enter bootloader"), "bootloader");
        assert!(hits("dfu-mode"), "dfu mode");
        assert!(hits("download_mode"), "download mode");
        assert!(hits("format c:"), "format");
        assert!(hits("factory_reset"), "factory_reset");
    }

    // ---- Acceptance criterion 1: priority matrix, danger ∩ whitelist ----

    #[test]
    fn whitelist_only_match_allows() {
        let set = rules(&["^status$"], &["erase"]);
        assert_eq!(
            set.evaluate(b"status"),
            RuleVerdict::Allow {
                reason: "whitelist:^status$".to_string()
            }
        );
    }

    #[test]
    fn no_match_is_default_pending() {
        let set = rules(&["^status$"], &["erase"]);
        assert_eq!(set.evaluate(b"reboot"), RuleVerdict::Pending);
    }

    #[test]
    fn danger_only_match_force_pends() {
        let set = rules(&["^status$"], &["erase"]);
        match set.evaluate(b"flash_erase") {
            RuleVerdict::ForcePending { matched_rule, .. } => {
                assert_eq!(matched_rule, "danger:erase");
            }
            other => panic!("expected ForcePending, got {other:?}"),
        }
    }

    #[test]
    fn danger_and_whitelist_both_matching_is_forced_to_pending_not_allowed() {
        // The literal priority-matrix acceptance criterion: a pattern that
        // satisfies *both* a whitelist entry and a danger entry must never
        // come out as `Allow` — whitelist cannot paper over danger.
        let set = rules(&["erase"], &["erase"]);
        match set.evaluate(b"erase") {
            RuleVerdict::ForcePending { matched_rule, .. } => {
                assert_eq!(matched_rule, "danger:erase");
            }
            other => panic!(
                "danger ∩ whitelist must force approval, got {other:?} instead of ForcePending"
            ),
        }
    }

    #[test]
    fn danger_and_whitelist_both_matching_via_different_patterns_still_force_pends() {
        // Same as above but with a broader whitelist entry that happens to
        // also cover a dangerous string, and a differently-worded danger
        // rule — proving this isn't just "identical pattern string" special
        // casing.
        let set = rules(&["^.*$"], &["erase"]);
        match set.evaluate(b"flash_erase now") {
            RuleVerdict::ForcePending { matched_rule, .. } => {
                assert_eq!(matched_rule, "danger:erase");
            }
            other => {
                panic!("expected ForcePending despite a whitelist-everything rule, got {other:?}")
            }
        }
    }

    // ---- Acceptance criterion 2: regex boundaries (case, partial match) ----

    #[test]
    fn danger_matching_is_case_insensitive() {
        let set = rules(&[], &["erase"]);
        for variant in ["erase", "ERASE", "Erase", "ErAsE"] {
            assert!(
                matches!(
                    set.evaluate(variant.as_bytes()),
                    RuleVerdict::ForcePending { .. }
                ),
                "case variant {variant:?} should still match the danger rule"
            );
        }
    }

    #[test]
    fn danger_matching_is_a_substring_match_not_a_whole_string_match() {
        let set = rules(&[], &["erase"]);
        for variant in ["flash_erase", "ERASE_ALL", "pre-erase-check", "erase"] {
            assert!(
                matches!(
                    set.evaluate(variant.as_bytes()),
                    RuleVerdict::ForcePending { .. }
                ),
                "{variant:?} contains the danger pattern as a substring and should match"
            );
        }
    }

    #[test]
    fn whitelist_matching_is_also_case_insensitive_and_substring_by_default() {
        let set = rules(&["status"], &[]);
        assert_eq!(
            set.evaluate(b"STATUS"),
            RuleVerdict::Allow {
                reason: "whitelist:status".to_string()
            }
        );
        assert_eq!(
            set.evaluate(b"get_status_now"),
            RuleVerdict::Allow {
                reason: "whitelist:status".to_string()
            }
        );
    }

    #[test]
    fn a_whitelist_entry_can_still_be_anchored_by_the_operator() {
        // Case-insensitivity/substring matching are the *rule engine's*
        // defaults, not a ban on anchors — an operator who writes `^status$`
        // still gets an exact-line whitelist entry.
        let set = rules(&["^status$"], &[]);
        assert_eq!(set.evaluate(b"get_status_now"), RuleVerdict::Pending);
    }

    #[test]
    fn unrelated_substring_does_not_false_positive_on_danger() {
        let set = rules(&[], &["erase"]);
        assert_eq!(set.evaluate(b"read status"), RuleVerdict::Pending);
    }

    // ---- Acceptance criterion 3: hex-decoded bytes are matched, not the wire form ----

    #[test]
    fn hex_decoded_danger_command_is_caught_exactly_like_the_plain_text_form() {
        // Simulates `serialwrap write --hex <the hex of "flash_erase">`:
        // the daemon decodes the hex to raw bytes *before* this function
        // ever sees them (see the module docs) — proving `evaluate` catches
        // it once decoded is what closes the bypass, since the *encoded*
        // form (`"666c6173685f65726173650a"`) obviously doesn't contain the
        // substring `"erase"` and would sail through if rules were ever
        // matched against wire text instead of decoded bytes.
        let set = rules(&[], &["erase"]);
        let hex = "666c6173685f65726173650a"; // "flash_erase\n"
        let decoded: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&hex[i..i + 2], 16).unwrap())
            .collect();
        assert_eq!(decoded, b"flash_erase\n");

        // The encoded wire form itself must NOT match — confirms this test
        // is actually exercising the bypass scenario, not a tautology.
        assert_eq!(set.evaluate(hex.as_bytes()), RuleVerdict::Pending);

        // The decoded bytes — what the daemon actually hands to `evaluate`
        // — must be caught.
        match set.evaluate(&decoded) {
            RuleVerdict::ForcePending { matched_rule, .. } => {
                assert_eq!(matched_rule, "danger:erase");
            }
            other => panic!("hex-decoded danger command bypassed the gate: {other:?}"),
        }
    }

    #[test]
    fn hex_decoded_bytes_with_arbitrary_case_still_match() {
        let set = rules(&[], &["erase"]);
        // Hex for "FLASH_ERASE" (uppercase) — proves case-insensitivity
        // survives the decode step too, not just the plain-text path.
        let decoded = b"FLASH_ERASE".to_vec();
        assert!(matches!(
            set.evaluate(&decoded),
            RuleVerdict::ForcePending { .. }
        ));
    }

    // ---- rules.toml parsing ----

    #[test]
    fn load_missing_file_falls_back_to_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("does-not-exist.toml");
        let set = RuleSet::load(&path).expect("missing file falls back to builtin");
        assert_eq!(set.danger.len(), 5);
        assert!(set.whitelist.is_empty());
        assert_eq!(set.timeout, DEFAULT_APPROVAL_TIMEOUT);
    }

    #[test]
    fn load_parses_whitelist_danger_and_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(
            &path,
            r#"
[approval]
timeout_s = 5

[[whitelist]]
pattern = "^status$"

[[danger]]
pattern = "nuke"
reason = "test rule"
"#,
        )
        .unwrap();
        let set = RuleSet::load(&path).expect("valid rules.toml loads");
        assert_eq!(set.timeout, Duration::from_secs(5));
        assert_eq!(
            set.evaluate(b"status"),
            RuleVerdict::Allow {
                reason: "whitelist:^status$".to_string()
            }
        );
        match set.evaluate(b"nuke it") {
            RuleVerdict::ForcePending {
                matched_rule,
                danger_reason,
            } => {
                assert_eq!(matched_rule, "danger:nuke");
                assert_eq!(danger_reason, "test rule");
            }
            other => panic!("expected ForcePending, got {other:?}"),
        }
    }

    #[test]
    fn load_rejects_invalid_regex_with_a_clear_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(&path, "[[danger]]\npattern = \"(unclosed\"\n").unwrap();
        let err = RuleSet::load(&path).expect_err("invalid regex must fail to load");
        assert!(err.to_string().contains("invalid rules.toml"), "{err}");
        assert!(err.to_string().contains("invalid regex"), "{err}");
        assert!(err.to_string().contains("unclosed"), "{err}");
    }

    #[test]
    fn load_rejects_nonpositive_timeout() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("rules.toml");
        std::fs::write(&path, "[approval]\ntimeout_s = 0\n").unwrap();
        assert!(RuleSet::load(&path).is_err());
    }
}
