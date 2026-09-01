use wsx_core::integration::resume::{plan, AgentResumePlan};
use wsx_core::runtime::{AgentSessionRef, AgentSessionRefKind};

fn id(value: &str) -> AgentSessionRef {
    AgentSessionRef::id(value.to_owned()).expect("test ID must be valid")
}

fn path(value: &str) -> AgentSessionRef {
    AgentSessionRef::path(value.to_owned()).expect("test path must be valid")
}

fn planned(provider: &str, session_ref: &AgentSessionRef) -> AgentResumePlan {
    plan(provider, session_ref).unwrap_or_else(|| panic!("{provider} should support this ref"))
}

fn absolute_path_with_bytes(bytes: usize) -> String {
    #[cfg(windows)]
    {
        assert!(bytes >= 3);
        format!("C:\\{}", "a".repeat(bytes - 3))
    }

    #[cfg(not(windows))]
    {
        assert!(bytes >= 1);
        format!("/{}", "a".repeat(bytes - 1))
    }
}

fn multibyte_absolute_path_with_bytes(bytes: usize) -> String {
    #[cfg(windows)]
    let prefix = "C:\\";
    #[cfg(not(windows))]
    let prefix = "/";
    #[cfg(windows)]
    let separator = "\\";
    #[cfg(not(windows))]
    let separator = "/";

    assert!(bytes >= prefix.len());
    let remaining = bytes - prefix.len();
    let segment_count = (remaining + 1) / ("é".len() + separator.len());
    let mut value = prefix.to_owned();
    if segment_count > 0 {
        value.push_str(&vec!["é"; segment_count].join(separator));
    }
    value.push_str(&"x".repeat(bytes - value.len()));
    assert_eq!(value.len(), bytes);
    value
}

fn valid_absolute_path() -> String {
    absolute_path_with_bytes(32)
}

#[test]
fn session_refs_validate_content_and_byte_limits() {
    assert!(matches!(id("resume-123").kind, AgentSessionRefKind::Id));
    assert!(matches!(
        path(&valid_absolute_path()).kind,
        AgentSessionRefKind::Path
    ));

    assert!(AgentSessionRef::id(String::new()).is_none());
    assert!(AgentSessionRef::id("x".repeat(512)).is_some());
    assert!(AgentSessionRef::id("x".repeat(513)).is_none());
    assert!(AgentSessionRef::id("é".repeat(256)).is_some());
    assert!(AgentSessionRef::id("é".repeat(257)).is_none());
    assert!(AgentSessionRef::id("has\nnewline".to_owned()).is_none());
    assert!(AgentSessionRef::id("has\u{7f}delete".to_owned()).is_none());
    assert!(AgentSessionRef::id("has\0nul".to_owned()).is_none());
    assert!(AgentSessionRef::id("has\ttab".to_owned()).is_none());
    assert!(AgentSessionRef::id("--dangerously-skip-permissions").is_none());
    assert!(AgentSessionRef::id("-session").is_none());

    assert!(AgentSessionRef::path(String::new()).is_none());
    assert!(AgentSessionRef::path("relative/session".to_owned()).is_none());
    assert!(AgentSessionRef::path(valid_absolute_path()).is_some());
    assert!(AgentSessionRef::path(absolute_path_with_bytes(4096)).is_some());
    assert!(AgentSessionRef::path(absolute_path_with_bytes(4097)).is_none());
    assert!(AgentSessionRef::path(multibyte_absolute_path_with_bytes(4095)).is_some());
    assert!(AgentSessionRef::path(multibyte_absolute_path_with_bytes(4096)).is_some());
    assert!(AgentSessionRef::path(multibyte_absolute_path_with_bytes(4097)).is_none());
    assert!(AgentSessionRef::path(format!("{}\n", valid_absolute_path())).is_none());
    assert!(AgentSessionRef::path(format!("{}\0", valid_absolute_path())).is_none());
    assert!(AgentSessionRef::path(format!("{}\t", valid_absolute_path())).is_none());
}

#[test]
fn plan_maps_every_supported_id_provider_to_exact_argv() {
    let cases: &[(&str, &[&str])] = &[
        ("pi", &["pi", "--session"]),
        ("omp", &["omp", "--resume="]),
        ("claude", &["claude", "--resume"]),
        ("codex", &["codex", "resume"]),
        ("copilot", &["copilot", "--resume="]),
        ("devin", &["devin", "--resume"]),
        ("droid", &["droid", "--resume"]),
        ("kimi", &["kimi", "--session"]),
        ("opencode", &["opencode", "--session"]),
        ("kilo", &["kilo", "--session"]),
        ("hermes", &["hermes", "--resume"]),
        ("qodercli", &["qodercli", "--resume"]),
        ("qwen", &["qwen", "--resume"]),
        (
            "cursor",
            &[
                if cfg!(windows) {
                    "cursor-agent.cmd"
                } else {
                    "cursor-agent"
                },
                "--resume",
            ],
        ),
        ("mastracode", &["mastracode", "--thread"]),
        ("antigravity-cli", &["agy", "--conversation"]),
        ("grok", &["grok", "--resume"]),
    ];

    for (provider, prefix) in cases {
        let reference = id("resume id; still one argv value");
        let mut expected: Vec<String> = prefix.iter().map(|part| (*part).to_owned()).collect();
        if matches!(*provider, "omp" | "copilot") {
            expected[1].push_str("resume id; still one argv value");
        } else {
            expected.push("resume id; still one argv value".to_owned());
        }

        let result = planned(provider, &reference);
        assert_eq!(result.argv, expected, "provider {provider}");
    }
}

#[test]
fn pi_and_omp_accept_paths_and_all_other_providers_reject_them() {
    let value = valid_absolute_path();
    let reference = path(&value);

    assert_eq!(
        planned("pi", &reference).argv,
        vec!["pi".to_owned(), "--session".to_owned(), value.clone()]
    );
    assert_eq!(
        planned("omp", &reference).argv,
        vec!["omp".to_owned(), format!("--resume={value}")]
    );

    for provider in [
        "claude",
        "codex",
        "copilot",
        "devin",
        "droid",
        "kimi",
        "opencode",
        "kilo",
        "hermes",
        "qodercli",
        "qwen",
        "cursor",
        "mastracode",
        "antigravity-cli",
        "grok",
    ] {
        assert!(
            plan(provider, &reference).is_none(),
            "{provider} must reject paths"
        );
    }
}

#[test]
fn unknown_providers_and_invalid_provider_ref_pairs_have_no_plan() {
    assert!(plan("unknown-provider", &id("resume-1")).is_none());
    assert!(plan("", &id("resume-1")).is_none());
    assert!(plan("codex", &path(&valid_absolute_path())).is_none());
}

#[test]
fn dedupe_keys_track_provider_ref_kind_and_ref_value() {
    let pi_id_one = planned("pi", &id("one"));
    let pi_id_one_again = planned("pi", &id("one"));
    let omp_id_one = planned("omp", &id("one"));
    let pi_id_two = planned("pi", &id("two"));
    let pi_path_one = planned("pi", &path(&valid_absolute_path()));

    assert_eq!(pi_id_one.dedupe_key, pi_id_one_again.dedupe_key);
    assert_ne!(pi_id_one.dedupe_key, omp_id_one.dedupe_key);
    assert_ne!(pi_id_one.dedupe_key, pi_id_two.dedupe_key);
    assert_ne!(pi_id_one.dedupe_key, pi_path_one.dedupe_key);
}
